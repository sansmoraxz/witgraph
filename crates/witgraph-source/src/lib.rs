//! Resolving the nodes a graph runs, from files and OCI registries, and
//! pushing them.
//!
//! A graph's component table names each component and where its node is: a
//! path to its node manifest (relative to the graph), or an OCI node
//! artifact, written explicitly as `oci://registry/repository:tag` (or
//! `@sha256:…`, pinned).
//!
//! # Nodes
//!
//! A node is a component: what it is (its [`NodeDescription`]: the WIT
//! package its worlds are in, the worlds it provides, how an editor previews
//! their outputs), its WIT, and its builds. Its contracts are lowered from
//! the WIT, never read out of a build, which is only checked to target every
//! world the description lists ([`verify_component`]).
//!
//! As files, a [`LocalManifest`] (`*.node.json`) says what it is and names
//! its WIT and builds by paths relative to it:
//!
//! ```json
//! {
//!   "wit": "demo:graph@0.1.0",
//!   "wit_source": "wit",
//!   "worlds": ["demo:graph/sensor@0.1.0"],
//!   "component": { "release": "out/sensor.wasm", "debug": "out/sensor.debug.wasm" },
//!   "viewers": { "demo:graph/sensor@0.1.0": { "frames": "image" } }
//! }
//! ```
//!
//! `wit_source` is a WIT directory or file (with its `deps/`, as `wkg fetch`
//! fills it), or the package encoded, as `wkg build` writes it. `component`
//! is one build, or one per [`Profile`].
//!
//! Over OCI, a node is an image index (see [`oci`]) of Wasm OCI artifacts:
//! its WIT package, as `wkg publish` pushes one, and its builds, as `wkg oci
//! push` pushes them; its description is in the index's annotations. Only
//! standard media types are used. [`Resolver::push`] packs a node's files
//! into one.
//!
//! # Profiles
//!
//! A node may have a build per profile: `release`, and `debug`, built with
//! debug info and without optimizing, which a debugger stops on the lines
//! of. A resolver pulls the build for its [`profile`](Resolver::profile)
//! only; a node of one build has it used whatever the profile.
//!
//! # Cache
//!
//! With a cache directory ([`Resolver::cache`]), everything pulled is stored
//! under its digest, and each tag with the digest of the node it held. A
//! node pinned by digest is then read from disk without contacting the
//! registry again; a tag is pulled, and if its registry cannot be reached,
//! the node it held when last pulled is used, marked
//! [`offline`](Component::offline). Cached content is checked against its
//! digest.

mod files;
mod local;
pub mod oci;
mod push;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use oci_client::client::{ClientConfig, ClientProtocol};
pub use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use witgraph_ir::{
    Capability, ComponentContract, ComponentName, ContractId, ContractSource, NodeDescription,
    WitRef, WorldRef,
};
use witgraph_wit::load::WitSource;
use witgraph_wit::verify::{Imports, verify_component};

pub use local::{Builds, LocalManifest};
pub use push::Pushed;

/// The prefix of an OCI location.
pub const OCI_SCHEME: &str = "oci://";

/// The sha-256 of a component's bytes, written `sha256:<64 lowercase hex
/// digits>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// The 64 hex digits, without the `sha256:` prefix.
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.0)
    }
}

/// A string that is not `sha256:` followed by 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a digest: expected `sha256:` and 64 lowercase hex digits")]
pub struct ParseDigestError(String);

impl FromStr for Digest {
    type Err = ParseDigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.strip_prefix("sha256:") {
            Some(hex)
                if hex.len() == 64
                    && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Ok(Self(hex.to_string()))
            }
            _ => Err(ParseDigestError(s.to_string())),
        }
    }
}

impl TryFrom<String> for Digest {
    type Error = ParseDigestError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> Self {
        digest.to_string()
    }
}

/// Where a node is, as a graph's component table names it: its node
/// manifest's path, or an OCI node artifact.
///
/// Written as a string: `oci://registry/repository:tag` (or `@sha256:…`)
/// for an artifact, anything else a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Location {
    /// A path, relative to the graph's directory ([`Resolver::base`]).
    File(PathBuf),
    /// An OCI reference (`registry/repository:tag`), without the scheme.
    Oci(String),
}

impl Location {
    /// Whether this is an OCI reference pinned by digest: one that names
    /// the same artifact for good.
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Oci(reference) if reference.contains("@sha256:"))
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Oci(reference) => write!(f, "{OCI_SCHEME}{reference}"),
        }
    }
}

impl FromStr for Location {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("an empty location".into());
        }
        Ok(match s.strip_prefix(OCI_SCHEME) {
            Some(reference) => Self::Oci(reference.to_string()),
            None => Self::File(PathBuf::from(s)),
        })
    }
}

impl TryFrom<String> for Location {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Location> for String {
    fn from(location: Location) -> Self {
        location.to_string()
    }
}

/// Which build of a node is run.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Built to run.
    #[default]
    Release,
    /// Built with debug info and without optimizing, to stop on its lines.
    Debug,
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Release => "release",
            Self::Debug => "debug",
        })
    }
}

impl FromStr for Profile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "release" => Ok(Self::Release),
            "debug" => Ok(Self::Debug),
            _ => Err(format!("`{s}` is not a profile: `release` or `debug`")),
        }
    }
}

/// Why a node could not be resolved or pushed.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// What went wrong resolving one component of a table.
    #[error("component `{component}`: {error}")]
    In {
        /// The component.
        component: ComponentName,
        /// What went wrong.
        #[source]
        error: Box<SourceError>,
    },
    /// A location in the component table does not parse.
    #[error("`{location}` is not a location: {message}")]
    Location {
        /// The location as written.
        location: String,
        /// Why.
        message: String,
    },
    /// A file could not be read.
    #[error("failed to read `{}`", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// An OCI reference does not parse, or names what it cannot.
    #[error("`{reference}` is not an OCI reference to use: {message}")]
    Reference {
        /// The reference.
        reference: String,
        /// Why.
        message: String,
    },
    /// Pulling from a registry failed.
    #[error("failed to pull `{reference}`: {message}")]
    Pull {
        /// What was pulled.
        reference: String,
        /// The registry client's error.
        message: String,
    },
    /// A registry could not be reached.
    #[error("failed to reach the registry of `{reference}`: {message}")]
    Unreachable {
        /// What was pulled.
        reference: String,
        /// The registry client's error.
        message: String,
    },
    /// Pushing to a registry failed.
    #[error("failed to push to `{reference}`: {message}")]
    Push {
        /// Where it was pushed.
        reference: String,
        /// The registry client's error.
        message: String,
    },
    /// An OCI artifact is not a node: not an image index with a description.
    #[error(
        "`{reference}` is not a node: not an image index annotated `{}`",
        oci::NODE_ANNOTATION
    )]
    NotANode {
        /// The artifact.
        reference: String,
    },
    /// A node artifact is not as a node artifact is made.
    #[error("the node artifact `{reference}`: {message}")]
    Artifact {
        /// The artifact.
        reference: String,
        /// What is wrong.
        message: String,
    },
    /// A node has no build for the profile asked for.
    #[error("`{location}` has no {profile} build{}", builds(found))]
    NoBuild {
        /// The node.
        location: Location,
        /// The profile asked for.
        profile: Profile,
        /// The profiles it has builds for.
        found: Vec<Profile>,
    },
    /// A path outside [`Resolver::base`]: absolute, leaving it through
    /// `..`, or leading out of it through a symbolic link (see
    /// [`Resolver::allow_paths_outside_base`]).
    #[error("`{}` is outside the base directory", path.display())]
    OutsideBase {
        /// The path as named.
        path: PathBuf,
    },
    /// A file or blob is larger than [`Resolver::max_component_bytes`].
    #[error("`{location}` is over the limit of {limit} bytes")]
    TooLarge {
        /// Where it is.
        location: Location,
        /// The limit.
        limit: u64,
    },
    /// A node manifest is not one.
    #[error("`{location}` is not a node manifest: {message}")]
    Manifest {
        /// Where it is.
        location: Location,
        /// Why.
        message: String,
    },
    /// A node's WIT does not load, or is not its `wit` package.
    #[error("the WIT at `{location}`: {message}")]
    Wit {
        /// Where it is.
        location: Location,
        /// What is wrong.
        message: String,
    },
    /// A world a node lists is not in its `wit` package, or does not lower.
    #[error("world `{world}`: {message}")]
    World {
        /// The world.
        world: Box<WorldRef>,
        /// What is wrong.
        message: String,
    },
    /// A build does not target a world the node lists.
    #[error("{0}")]
    Verify(#[from] witgraph_wit::verify::VerifyError),
}

/// `: it has <profiles> builds`, or that it has one, for [`SourceError::NoBuild`].
fn builds(found: &[Profile]) -> String {
    match found {
        [] => String::new(),
        found => format!(
            ": it has {}",
            found
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" and ")
        ),
    }
}

impl SourceError {
    fn of(self, component: &ComponentName) -> Self {
        Self::In {
            component: component.clone(),
            error: Box::new(self),
        }
    }
}

/// A node fetched, with the build to run: before it is checked and lowered.
pub(crate) struct Node {
    pub(crate) description: NodeDescription,
    pub(crate) wit: WitSource,
    pub(crate) wit_location: Location,
    pub(crate) profile: Option<Profile>,
    pub(crate) bytes: Bytes,
    pub(crate) digest: Digest,
    pub(crate) artifact: Option<Digest>,
    pub(crate) offline: bool,
}

/// One component, resolved: a node, with the build it runs.
#[derive(Debug, Clone)]
pub struct Component {
    /// Where the graph's table says its node is.
    pub location: Location,
    /// What it is.
    pub description: NodeDescription,
    /// Which build this is: none when the node has one only.
    pub profile: Option<Profile>,
    /// The build.
    pub bytes: Bytes,
    /// The sha-256 of `bytes`.
    pub digest: Digest,
    /// The node artifact's digest, when it was pulled: what pins it.
    pub artifact: Option<Digest>,
    /// Its WIT.
    pub wit: WitSource,
    /// The names its build imports.
    pub imports: Imports,
    /// The contract of every node world it provides, in description order,
    /// its viewers applied.
    pub contracts: Vec<ComponentContract>,
    /// What every world it provides imports, as its WIT declares it.
    pub capabilities: Vec<Capability>,
    /// What was wrong but did not stop it resolving: viewers skipped.
    pub warnings: Vec<String>,
    /// Whether it is the node a tag held when last pulled, as its registry
    /// could not be reached.
    pub offline: bool,
}

impl Component {
    /// Its WIT, as one wasm-encoded package: how it travels (to an editor
    /// in a browser, say).
    pub fn wit_encoded(&self) -> Result<Vec<u8>, witgraph_wit::load::LoadError> {
        witgraph_wit::load::encode(&self.wit)
    }

    /// Its build with its WIT: what a runtime builds a graph from.
    pub fn artifact(&self) -> witgraph_wit::Artifact<'_> {
        witgraph_wit::Artifact {
            bytes: &self.bytes,
            wit: &self.wit,
            capabilities: &self.capabilities,
        }
    }
}

/// Every component of a graph's table, resolved, by name.
#[derive(Debug, Clone, Default)]
pub struct Components(BTreeMap<ComponentName, Component>);

impl Components {
    /// Each component's bytes with its WIT, by name: what
    /// `witgraph_runtime::RuntimeGraph::load` takes.
    pub fn artifacts(&self) -> HashMap<ComponentName, witgraph_wit::Artifact<'_>> {
        self.0
            .iter()
            .map(|(name, component)| (name.clone(), component.artifact()))
            .collect()
    }

    /// The component named `name`.
    pub fn get(&self, name: &ComponentName) -> Option<&Component> {
        self.0.get(name)
    }

    /// Every component, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&ComponentName, &Component)> {
        self.0.iter()
    }

    /// How many components were resolved.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none was.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Adds `component` under `name`, in place of any there.
    pub fn insert(&mut self, name: ComponentName, component: Component) {
        self.0.insert(name, component);
    }
}

impl FromIterator<(ComponentName, Component)> for Components {
    fn from_iter<I: IntoIterator<Item = (ComponentName, Component)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl ContractSource for Components {
    fn contract(&self, id: &ContractId) -> Option<&ComponentContract> {
        self.0
            .get(&id.component)?
            .contracts
            .iter()
            .find(|c| &c.id == id)
    }
}

/// Fetches nodes, from files and OCI registries, and pushes them.
pub struct Resolver {
    base: PathBuf,
    cache: Option<PathBuf>,
    /// Credentials, by registry (`host[:port]`). Every other registry is
    /// reached anonymously.
    auth: HashMap<String, RegistryAuth>,
    /// Whether a file may be named outside `base`.
    any_path: bool,
    /// One registry client for every pull, so its connections and tokens
    /// are reused.
    client: Client,
    /// The most bytes one build may take.
    max_bytes: u64,
    /// Which build of each node is resolved.
    profile: Profile,
}

/// The most sources [`Resolver::resolve`] fetches at once.
const FETCHES: usize = 8;

/// The most bytes a component may take, unless
/// [`Resolver::max_component_bytes`] says otherwise.
pub const DEFAULT_MAX_COMPONENT_BYTES: u64 = 256 << 20;

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the credentials.
        f.debug_struct("Resolver")
            .field("base", &self.base)
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl Resolver {
    /// A resolver with no cache and anonymous registry access, resolving
    /// relative file paths against the working directory.
    pub fn new() -> Self {
        Self {
            base: PathBuf::new(),
            cache: None,
            auth: HashMap::new(),
            any_path: false,
            client: client(ClientProtocol::Https),
            max_bytes: DEFAULT_MAX_COMPONENT_BYTES,
            profile: Profile::Release,
        }
    }

    /// Resolves the build of each node for `profile` (`release` by
    /// default): a node with builds for others only fails to resolve
    /// ([`SourceError::NoBuild`]).
    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// The most bytes one build may take
    /// ([`DEFAULT_MAX_COMPONENT_BYTES`] by default). A larger file or layer
    /// is refused ([`SourceError::TooLarge`]) before it is read whole.
    pub fn max_component_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Resolves relative paths against `dir` (the graph's directory, say).
    pub fn base(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base = dir.into();
        self
    }

    /// Keeps what is pulled in `dir`, by digest (see the
    /// [crate docs](crate#cache)). The directory is created when first
    /// written.
    pub fn cache(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache = Some(dir.into());
        self
    }

    /// The credentials presented to `registry` (`host[:port]`, as an OCI
    /// reference names it), and to no other: a graph naming another
    /// registry never sees them. Registries without credentials are
    /// reached anonymously.
    pub fn auth(mut self, registry: impl Into<String>, auth: RegistryAuth) -> Self {
        self.auth.insert(registry.into(), auth);
        self
    }

    /// Lets a graph and its node manifests name any path: absolute ones, ones leaving
    /// [`base`](Self::base) through `..`, and ones whose symbolic links
    /// lead out of it. By default they may not
    /// ([`SourceError::OutsideBase`]), so a graph from elsewhere
    /// cannot read files beside the project; a confined file is read by
    /// the path its links resolved to when it was checked, and refused if what
    /// was opened is not what that path holds, so a link swapped in after the
    /// check is not followed.
    pub fn allow_paths_outside_base(mut self) -> Self {
        self.any_path = true;
        self
    }

    /// Reaches the listed registries (`host:port`) over plain HTTP, for a
    /// local registry. Every other registry is reached over HTTPS.
    pub fn insecure_registries(mut self, registries: Vec<String>) -> Self {
        self.client = client(ClientProtocol::HttpsExcept(registries));
        self
    }

    /// Resolves every component of a graph's component table (name to
    /// node location), several at once. Fails if any cannot be
    /// resolved, with the error of the first by name.
    pub async fn resolve(
        &self,
        table: &BTreeMap<ComponentName, String>,
    ) -> Result<Components, SourceError> {
        let resolved: Vec<(ComponentName, Component)> =
            futures::stream::iter(table.iter().map(|(name, location)| async move {
                let component = self
                    .component(name, location)
                    .await
                    .map_err(|e| e.of(name))?;
                Ok::<_, SourceError>((name.clone(), component))
            }))
            .buffered(FETCHES)
            .try_collect()
            .await?;
        Ok(Components(resolved.into_iter().collect()))
    }

    /// Resolves one component, `name` in a graph's table, from the node at
    /// `location`.
    pub async fn component(
        &self,
        name: &ComponentName,
        location: &str,
    ) -> Result<Component, SourceError> {
        let location: Location = location.parse().map_err(|message| SourceError::Location {
            location: location.to_string(),
            message,
        })?;
        let node = match &location {
            Location::File(path) => self.local(path).await?,
            Location::Oci(reference) => self.pull_node(reference).await?,
        };
        describe(name, &location, node)
    }

    /// The credentials for the registry `reference` names.
    fn auth_for(&self, reference: &Reference) -> &RegistryAuth {
        static ANONYMOUS: RegistryAuth = RegistryAuth::Anonymous;
        self.auth
            .get(reference.resolve_registry())
            .or_else(|| self.auth.get(reference.registry()))
            .unwrap_or(&ANONYMOUS)
    }
}

/// `node`, checked and lowered, as component `name` at `location`: its WIT is
/// its `wit` package, every world it lists is in it and targeted by its
/// build, and the node worlds lower.
pub(crate) fn describe(
    name: &ComponentName,
    location: &Location,
    node: Node,
) -> Result<Component, SourceError> {
    let Node {
        description,
        wit,
        wit_location,
        profile,
        bytes,
        digest,
        artifact,
        offline,
    } = node;
    let wit_error = |message: String| SourceError::Wit {
        location: wit_location.clone(),
        message,
    };
    let roots: Vec<WitRef> = wit
        .packages
        .iter()
        .map(|&id| {
            let name = &wit.resolve.packages[id].name;
            WitRef {
                namespace: name.namespace.clone(),
                name: name.name.clone(),
                version: name.version.clone(),
            }
        })
        .collect();
    if !roots.contains(&description.wit) {
        return Err(wit_error(format!(
            "it is not the package `{}` the node names",
            description.wit
        )));
    }

    let mut imports = Imports::default();
    let mut contracts = Vec::new();
    let mut capabilities = Vec::new();
    let mut warnings = Vec::new();
    for world in &description.worlds {
        let world_error = |message: String| SourceError::World {
            world: Box::new(world.clone()),
            message,
        };
        if world.package != description.wit {
            return Err(world_error(format!(
                "it is not in the package `{}`",
                description.wit
            )));
        }
        let Some(id) = witgraph_wit::lower::find_world(&wit, world) else {
            return Err(world_error("the WIT has no such world".into()));
        };
        imports = verify_component(&wit, world, &bytes)?;
        Capability::add_unique(
            &mut capabilities,
            &witgraph_wit::lower::world_capabilities(&wit, id),
        );
        match witgraph_wit::lower_world(&wit, &ContractId::new(name.clone(), world.clone())) {
            Ok(lowered) => {
                let mut contract = lowered.contract;
                if let Some(hints) = description.viewers.get(world) {
                    warnings.extend(witgraph_wit::viewer::apply(&mut contract, hints));
                }
                contracts.push(contract);
            }
            // A provider's world.
            Err(witgraph_wit::Error::NotANode { .. }) => {}
            Err(e) => return Err(world_error(e.to_string())),
        }
    }
    for world in description.viewers.keys() {
        if !description.worlds.contains(world) {
            warnings.push(format!(
                "viewers are given for `{world}`, which the component does not provide"
            ));
        }
    }
    Ok(Component {
        location: location.clone(),
        description,
        profile,
        bytes,
        digest,
        artifact,
        wit,
        imports,
        contracts,
        capabilities,
        warnings,
        offline,
    })
}

/// A registry client speaking `protocol`.
fn client(protocol: ClientProtocol) -> Client {
    Client::new(ClientConfig {
        protocol,
        ..ClientConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn a_digest_is_the_sha256_of_the_bytes() {
        assert_eq!(Digest::of(b"").to_string(), EMPTY);
        assert_eq!(EMPTY.parse::<Digest>().unwrap(), Digest::of(b""));
    }

    #[test]
    fn malformed_digests_are_rejected() {
        for bad in [
            "",
            "sha256:",
            "sha256:abc",
            &EMPTY.to_uppercase(),
            &EMPTY.replace("sha256", "sha512"),
            &format!("{EMPTY}0"),
        ] {
            assert!(bad.parse::<Digest>().is_err(), "{bad}");
        }
    }

    #[test]
    fn locations_are_explicit_about_oci() {
        let oci: Location = "oci://ghcr.io/demo/sensor:0.1.0".parse().unwrap();
        assert_eq!(oci, Location::Oci("ghcr.io/demo/sensor:0.1.0".into()));
        assert!(!oci.is_pinned());
        assert!(
            format!("oci://ghcr.io/demo/sensor@{EMPTY}")
                .parse::<Location>()
                .unwrap()
                .is_pinned()
        );
        let file: Location = "ghcr.io/demo/sensor.node.json".parse().unwrap();
        assert_eq!(file, Location::File("ghcr.io/demo/sensor.node.json".into()));
        assert_eq!(oci.to_string(), "oci://ghcr.io/demo/sensor:0.1.0");
        assert!("".parse::<Location>().is_err());
    }

    #[test]
    fn node_manifests_read_as_written() {
        let manifest: LocalManifest = serde_json::from_str(
            r#"{
                "wit": "demo:graph@0.1.0",
                "wit_source": "wit",
                "worlds": ["demo:graph/sensor@0.1.0"],
                "component": { "release": "sensor.wasm", "debug": "sensor.debug.wasm" },
                "viewers": { "demo:graph/sensor@0.1.0": { "frames": "image" } }
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.wit.to_string(), "demo:graph@0.1.0");
        assert_eq!(
            manifest.component.get(Profile::Debug),
            Some((
                Some(Profile::Debug),
                std::path::Path::new("sensor.debug.wasm")
            ))
        );
        assert_eq!(manifest.description().viewers.len(), 1);
        let back: LocalManifest =
            serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
        assert_eq!(back, manifest);
        let one: LocalManifest = serde_json::from_str(
            r#"{"wit":"d:g@0.1.0","wit_source":"w","worlds":[],"component":"c.wasm"}"#,
        )
        .unwrap();
        assert_eq!(
            one.component.get(Profile::Debug),
            Some((None, std::path::Path::new("c.wasm")))
        );
        let misspelled =
            r#"{"wit":"d:g@0.1.0","wit_source":"w","worlds":[],"component":"c","viewer":{}}"#;
        assert!(serde_json::from_str::<LocalManifest>(misspelled).is_err());
        let profile =
            r#"{"wit":"d:g@0.1.0","wit_source":"w","worlds":[],"component":{"relase":"c"}}"#;
        assert!(serde_json::from_str::<LocalManifest>(profile).is_err());
    }

    #[test]
    fn profiles_read_as_written() {
        assert_eq!("debug".parse::<Profile>(), Ok(Profile::Debug));
        assert_eq!(Profile::Release.to_string(), "release");
        assert!("Debug".parse::<Profile>().is_err());
        assert_eq!(Profile::default(), Profile::Release);
    }

    #[test]
    fn credentials_go_to_their_registry_only() {
        let basic = RegistryAuth::Basic("me".into(), "token".into());
        let resolver = Resolver::new().auth("ghcr.io", basic.clone());
        let mine: Reference = "ghcr.io/me/a:1".parse().unwrap();
        let theirs: Reference = "evil.example/x:1".parse().unwrap();
        assert_eq!(resolver.auth_for(&mine), &basic);
        assert_eq!(resolver.auth_for(&theirs), &RegistryAuth::Anonymous);
    }
}
