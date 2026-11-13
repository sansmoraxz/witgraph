//! Resolving the components a graph runs: their node manifests, the WIT
//! those name, and their bytes, from files and OCI registries.
//!
//! A graph's component table names each component and where its **node
//! manifest** is: a path (relative to the graph), or an OCI artifact,
//! written explicitly as `oci://registry/repository:tag` (or
//! `@sha256:…`, pinned). A node manifest says what the component is:
//!
//! ```json
//! {
//!   "wit": "demo:graph@0.1.0",
//!   "wit_source": "wit",
//!   "worlds": ["demo:graph/sensor@0.1.0"],
//!   "component": "sensor.wasm",
//!   "viewers": { "demo:graph/sensor@0.1.0": { "frames": "image" } }
//! }
//! ```
//!
//! - `wit`: the WIT package its worlds are in.
//! - `wit_source`: where that WIT is: a directory or `.wit` file (with its
//!   `deps/`), or a wasm-encoded WIT package, as a path or `oci://`.
//! - `worlds`: every world of `wit` the component provides. A node runs one
//!   of them; a link's provider need not export `node`.
//! - `component`: the component's bytes, as a path or `oci://`.
//! - `viewers`: how an editor previews outputs, per world.
//!
//! A manifest's paths are relative to it. One pulled over OCI may name only
//! `oci://` artifacts.
//!
//! A [`Resolver`] fetches all of it into [`Components`]: per component, its
//! bytes, its WIT, and the contract of each node world it provides, lowered
//! from the WIT, never read out of the bytes. The bytes are checked to
//! target every world the manifest lists
//! ([`verify_component`]).
//!
//! # Pinning
//!
//! An OCI reference with a digest (`oci://ghcr.io/demo/sensor@sha256:…`)
//! names one artifact for good; the registry serves only that. A tag may
//! move.
//!
//! # Cache
//!
//! With a cache directory ([`Resolver::cache`]), every blob pulled from a
//! registry is stored under its digest, and each pinned reference with the
//! digest of what it held, so a pinned reference is read from disk without
//! contacting the registry again. Cached bytes are checked against their
//! digest.
//!
//! # OCI artifacts
//!
//! An artifact holds exactly one layer of the kind wanted: a component or
//! an encoded WIT package in a WebAssembly layer
//! ([`WASM_LAYER_MEDIA_TYPES`]), as `wkg oci push` and `wash push` produce
//! them; a node manifest in a [`MANIFEST_MEDIA_TYPE`] layer. Layers of
//! other media types are ignored.

use std::collections::{BTreeMap, HashMap};

use futures::{StreamExt, TryStreamExt};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use oci_client::client::{ClientConfig, ClientProtocol};
pub use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use witgraph_ir::{
    Capability, ComponentContract, ComponentName, ContractId, ContractSource, WitRef, WorldRef,
};
use witgraph_wit::load::WitSource;
use witgraph_wit::verify::{Imports, verify_component};

/// The media type of a node manifest's layer, in an OCI artifact.
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.witgraph.node.v1+json";

/// The prefix of an OCI location.
pub const OCI_SCHEME: &str = "oci://";

/// The layer media types an OCI artifact may carry a component under.
pub const WASM_LAYER_MEDIA_TYPES: [&str; 3] = [
    "application/wasm",
    "application/vnd.wasm.content.layer.v1+wasm",
    "application/vnd.bytecodealliance.wasm.component.layer.v0+wasm",
];

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

/// Where something a component is made of is: a file (or, for WIT, a
/// directory), or an OCI artifact.
///
/// Written as a string: `oci://registry/repository:tag` (or `@sha256:…`)
/// for an artifact, anything else a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Location {
    /// A path. A relative one is resolved against what names it: the
    /// graph's directory ([`Resolver::base`]), or the node manifest's.
    File(PathBuf),
    /// An OCI image reference (`registry/repository:tag`), without the
    /// scheme.
    Oci(String),
}

impl Location {
    /// Whether this is an OCI reference pinned by digest: one that names
    /// the same artifact for good.
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Oci(reference) if reference.contains("@sha256:"))
    }

    /// `relative`, as a node manifest at `self` names it: a path is
    /// relative to the manifest's directory. A manifest pulled over OCI
    /// has no directory: it may only name artifacts.
    pub fn join(&self, relative: &Location) -> Result<Location, SourceError> {
        match (self, relative) {
            (_, Self::Oci(_)) => Ok(relative.clone()),
            (Self::File(manifest), Self::File(path)) if path.is_relative() => Ok(Self::File(
                manifest.parent().unwrap_or(Path::new("")).join(path),
            )),
            (Self::File(_), Self::File(_)) => Ok(relative.clone()),
            (Self::Oci(manifest), Self::File(path)) => Err(SourceError::FileFromOci {
                manifest: manifest.clone(),
                path: path.clone(),
            }),
        }
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

/// What a component is: a node manifest (see the [crate docs](crate)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeManifest {
    /// The WIT package the component's worlds are in.
    pub wit: WitRef,
    /// Where that WIT is: a directory or file, or an encoded WIT package.
    pub wit_source: Location,
    /// Every world of `wit` the component provides.
    pub worlds: Vec<WorldRef>,
    /// The component's bytes.
    pub component: Location,
    /// How an editor previews outputs: per world, output port to viewer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub viewers: BTreeMap<WorldRef, BTreeMap<String, String>>,
}

/// Why a component could not be resolved.
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
    /// An OCI reference does not parse.
    #[error("`{reference}` is not an OCI reference: {message}")]
    Reference {
        /// The reference as written.
        reference: String,
        /// Why it was rejected.
        message: String,
    },
    /// Pulling an OCI artifact failed.
    #[error("failed to pull `{reference}`: {message}")]
    Pull {
        /// The artifact.
        reference: String,
        /// The registry client's error.
        message: String,
    },
    /// An OCI artifact holds no layer of the kind wanted, or several.
    #[error("`{reference}` holds {found} {what} layers; it should hold one")]
    Layers {
        /// The artifact.
        reference: String,
        /// What kind of layer was wanted.
        what: &'static str,
        /// How many it holds.
        found: usize,
    },
    /// A path outside [`Resolver::base`]: absolute, leaving it through
    /// `..`, or leading out of it through a symbolic link (see
    /// [`Resolver::allow_paths_outside_base`]).
    #[error("`{}` is outside the base directory", path.display())]
    OutsideBase {
        /// The path as named.
        path: PathBuf,
    },
    /// A file or layer is larger than [`Resolver::max_component_bytes`].
    #[error("`{location}` is over the limit of {limit} bytes")]
    TooLarge {
        /// Where it is.
        location: Location,
        /// The limit.
        limit: u64,
    },
    /// A node manifest pulled over OCI names a path.
    #[error("the node manifest `{OCI_SCHEME}{manifest}` names the path `{}`: one pulled over OCI may name only `{OCI_SCHEME}` artifacts", path.display())]
    FileFromOci {
        /// The manifest's reference.
        manifest: String,
        /// The path it names.
        path: PathBuf,
    },
    /// A node manifest is not one.
    #[error("`{location}` is not a node manifest: {message}")]
    Manifest {
        /// Where it is.
        location: Location,
        /// Why.
        message: String,
    },
    /// The WIT a node manifest names does not load, or is not its `wit`
    /// package.
    #[error("the WIT at `{location}`: {message}")]
    Wit {
        /// Where it is.
        location: Location,
        /// What is wrong.
        message: String,
    },
    /// A world a node manifest lists is not in its `wit` package, or does
    /// not lower.
    #[error("world `{world}`: {message}")]
    World {
        /// The world.
        world: Box<WorldRef>,
        /// What is wrong.
        message: String,
    },
    /// The bytes do not target a world the manifest lists.
    #[error("{0}")]
    Verify(#[from] witgraph_wit::verify::VerifyError),
}

impl SourceError {
    fn of(self, component: &ComponentName) -> Self {
        Self::In {
            component: component.clone(),
            error: Box::new(self),
        }
    }
}

/// Bytes fetched, with their digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The bytes.
    pub bytes: Bytes,
    /// Their sha-256.
    pub digest: Digest,
}

/// One component, resolved: what its node manifest says it is.
#[derive(Debug, Clone)]
pub struct Component {
    /// Where its node manifest is.
    pub location: Location,
    /// Its node manifest.
    pub manifest: NodeManifest,
    /// The encoded component.
    pub bytes: Bytes,
    /// The sha-256 of `bytes`.
    pub digest: Digest,
    /// Its WIT.
    pub wit: WitSource,
    /// The names its bytes import.
    pub imports: Imports,
    /// The contract of every node world it provides, in manifest order,
    /// its viewers applied.
    pub contracts: Vec<ComponentContract>,
    /// What every world it provides imports, as its WIT declares it.
    pub capabilities: Vec<Capability>,
    /// What was wrong but did not stop it resolving: viewers skipped.
    pub warnings: Vec<String>,
}

impl Component {
    /// Its WIT, as one wasm-encoded package: how it travels (to an editor
    /// in a browser, say).
    pub fn wit_encoded(&self) -> Result<Vec<u8>, witgraph_wit::load::LoadError> {
        witgraph_wit::load::encode(&self.wit)
    }

    /// Its bytes with its WIT: what a runtime builds a graph from.
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

/// What kind of layer an artifact is pulled for.
#[derive(Debug, Clone, Copy)]
enum Layer {
    /// A component, or an encoded WIT package.
    Wasm,
    /// A node manifest.
    Manifest,
}

impl Layer {
    fn accepts(self, media_type: &str) -> bool {
        match self {
            Self::Wasm => WASM_LAYER_MEDIA_TYPES.contains(&media_type),
            Self::Manifest => media_type == MANIFEST_MEDIA_TYPE,
        }
    }

    fn what(self) -> &'static str {
        match self {
            Self::Wasm => "WebAssembly",
            Self::Manifest => "node manifest",
        }
    }
}

/// Fetches components: their node manifests, WIT, and bytes.
pub struct Resolver {
    base: PathBuf,
    cache: Option<PathBuf>,
    /// Credentials, by registry (`host[:port]`). Every other registry is
    /// reached anonymously.
    auth: HashMap<String, RegistryAuth>,
    /// Whether a file source may name a path outside `base`.
    any_path: bool,
    /// One registry client for every pull, so its connections and tokens
    /// are reused.
    client: Client,
    /// The most bytes one component may take.
    max_bytes: u64,
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
        }
    }

    /// The most bytes one component may take
    /// ([`DEFAULT_MAX_COMPONENT_BYTES`] by default). A larger file or layer
    /// is refused ([`SourceError::TooLarge`]) before it is read whole.
    pub fn max_component_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Resolves relative file paths against `dir` (the directory of the
    /// file the sources were read from, say).
    pub fn base(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base = dir.into();
        self
    }

    /// Keeps pulled components in `dir`, by digest (see the
    /// [crate docs](crate#cache)). The directory is created when first
    /// written.
    pub fn cache(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache = Some(dir.into());
        self
    }

    /// The credentials presented to `registry` (`host[:port]`, as an OCI
    /// reference names it), and to no other: a source list naming another
    /// registry never sees them. Registries without credentials are
    /// reached anonymously.
    pub fn auth(mut self, registry: impl Into<String>, auth: RegistryAuth) -> Self {
        self.auth.insert(registry.into(), auth);
        self
    }

    /// Lets file sources name any path: absolute ones, ones leaving
    /// [`base`](Self::base) through `..`, and ones whose symbolic links
    /// lead out of it. By default they may not
    /// ([`SourceError::OutsideBase`]), so a source list from elsewhere
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
    /// node manifest location), several at once. Fails if any cannot be
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

    /// Resolves one component, `name` in a graph's table, from the node
    /// manifest at `location`.
    pub async fn component(
        &self,
        name: &ComponentName,
        location: &str,
    ) -> Result<Component, SourceError> {
        let location: Location = location.parse().map_err(|message| SourceError::Location {
            location: location.to_string(),
            message,
        })?;
        let fetched = self.fetch_layer(&location, Layer::Manifest).await?;
        let manifest: NodeManifest =
            serde_json::from_slice(&fetched.bytes).map_err(|e| SourceError::Manifest {
                location: location.clone(),
                message: e.to_string(),
            })?;
        let bytes_at = location.join(&manifest.component)?;
        let wit_at = location.join(&manifest.wit_source)?;
        let (fetched, wit) = futures::join!(self.fetch(&bytes_at), self.wit(&wit_at));
        let (fetched, wit) = (fetched?, wit?);
        let wit_error = |message: String| SourceError::Wit {
            location: wit_at.clone(),
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
        if !roots.contains(&manifest.wit) {
            return Err(wit_error(format!(
                "it is not the package `{}` the manifest names",
                manifest.wit
            )));
        }

        let mut imports = Imports::default();
        let mut contracts = Vec::new();
        let mut capabilities = Vec::new();
        let mut warnings = Vec::new();
        for world in &manifest.worlds {
            let world_error = |message: String| SourceError::World {
                world: Box::new(world.clone()),
                message,
            };
            if world.package != manifest.wit {
                return Err(world_error(format!(
                    "it is not in the package `{}`",
                    manifest.wit
                )));
            }
            let Some(id) = witgraph_wit::lower::find_world(&wit, world) else {
                return Err(world_error("the WIT has no such world".into()));
            };
            imports = verify_component(&wit, world, &fetched.bytes)?;
            Capability::add_unique(
                &mut capabilities,
                &witgraph_wit::lower::world_capabilities(&wit, id),
            );
            match witgraph_wit::lower_world(&wit, &ContractId::new(name.clone(), world.clone())) {
                Ok(lowered) => {
                    let mut contract = lowered.contract;
                    if let Some(hints) = manifest.viewers.get(world) {
                        warnings.extend(witgraph_wit::viewer::apply(&mut contract, hints));
                    }
                    contracts.push(contract);
                }
                // A provider's world.
                Err(witgraph_wit::Error::NotANode { .. }) => {}
                Err(e) => return Err(world_error(e.to_string())),
            }
        }
        for world in manifest.viewers.keys() {
            if !manifest.worlds.contains(world) {
                warnings.push(format!(
                    "viewers are given for `{world}`, which the component does not provide"
                ));
            }
        }
        Ok(Component {
            location,
            manifest,
            bytes: fetched.bytes,
            digest: fetched.digest,
            wit,
            imports,
            contracts,
            capabilities,
            warnings,
        })
    }

    /// Loads the WIT at `location`: a directory or file, or an encoded
    /// package.
    async fn wit(&self, location: &Location) -> Result<WitSource, SourceError> {
        let failed = |message: String| SourceError::Wit {
            location: location.clone(),
            message,
        };
        match location {
            Location::File(path) => {
                let full = self.confined(path).await?;
                let loaded = tokio::task::spawn_blocking(move || {
                    witgraph_wit::load::load_path(full).map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| failed(e.to_string()))?;
                loaded.map_err(failed)
            }
            Location::Oci(_) => {
                let fetched = self.fetch(location).await?;
                witgraph_wit::load::load_encoded(&fetched.bytes).map_err(|e| failed(e.to_string()))
            }
        }
    }

    /// Fetches the file, or the one WebAssembly layer of the artifact, at
    /// `location`: a component, or an encoded WIT package.
    pub async fn fetch(&self, location: &Location) -> Result<Fetched, SourceError> {
        self.fetch_layer(location, Layer::Wasm).await
    }

    /// Fetches the file, or the one layer of the kind wanted of the
    /// artifact, at `location`.
    async fn fetch_layer(&self, location: &Location, layer: Layer) -> Result<Fetched, SourceError> {
        let too_large = || SourceError::TooLarge {
            location: location.clone(),
            limit: self.max_bytes,
        };
        match location {
            Location::File(path) => {
                let full = self.confined(path).await?;
                let file = open(&full).await?;
                if !self.any_path {
                    self.still_confined(path, &full, &file).await?;
                }
                let (bytes, digest) = read_file(file, &full, self.max_bytes)
                    .await?
                    .ok_or_else(too_large)?;
                Ok(Fetched {
                    bytes: bytes.into(),
                    digest,
                })
            }
            Location::Oci(reference) => {
                // Parsed first, so a malformed reference fails whether or
                // not it is cached.
                let parsed: Reference = reference.parse().map_err(|e| SourceError::Reference {
                    reference: reference.clone(),
                    message: format!("{e}"),
                })?;
                let pinned = location.is_pinned();
                if pinned && let Some(fetched) = self.cached_reference(reference).await {
                    return Ok(fetched);
                }
                let (bytes, digest) = self
                    .pull(reference, &parsed, layer)
                    .await?
                    .ok_or_else(too_large)?;
                let fetched = Fetched {
                    bytes: bytes.into(),
                    digest,
                };
                // Caching is best effort: the bytes are in hand whether or
                // not they could be kept.
                self.store(&fetched).await;
                if pinned {
                    self.store_reference(reference, &fetched.digest).await;
                }
                Ok(fetched)
            }
        }
    }

    /// The bytes cached for `digest`, if they are there and intact.
    pub async fn cached(&self, digest: &Digest) -> Option<Fetched> {
        let (bytes, found) = read(&self.cache_path(digest)?, self.max_bytes)
            .await
            .ok()??;
        // A damaged cache file is pulled again rather than trusted.
        (found == *digest).then(|| Fetched {
            bytes: bytes.into(),
            digest: found,
        })
    }

    /// What a pinned reference held when it was last pulled, from the cache.
    async fn cached_reference(&self, reference: &str) -> Option<Fetched> {
        let text = tokio::fs::read_to_string(self.reference_path(reference)?)
            .await
            .ok()?;
        self.cached(&text.trim().parse().ok()?).await
    }

    /// Notes in the cache what a pinned reference held.
    async fn store_reference(&self, reference: &str, digest: &Digest) {
        let Some(path) = self.reference_path(reference) else {
            return;
        };
        let text = digest.to_string();
        let write = move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, text);
        };
        let _ = tokio::task::spawn_blocking(write).await;
    }

    /// Where the cache notes what `reference` held.
    fn reference_path(&self, reference: &str) -> Option<PathBuf> {
        Some(
            self.cache
                .as_ref()?
                .join("references")
                .join(Digest::of(reference.as_bytes()).hex()),
        )
    }

    /// The path a file is read from: `path` under the
    /// base, refused unless it stays there once its symbolic links are
    /// followed (and then the path they lead to, so a link changed after
    /// this check is not followed). A file that is missing, or a link that
    /// leads nowhere, fails here, as its read would.
    async fn confined(&self, path: &Path) -> Result<PathBuf, SourceError> {
        let full = self.base.join(path);
        if self.any_path {
            return Ok(full);
        }
        let outside = || SourceError::OutsideBase {
            path: path.to_path_buf(),
        };
        let lexically_outside = path.is_absolute()
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
        if lexically_outside {
            return Err(outside());
        }
        // An empty base is the working directory.
        let base = match self.base.as_os_str().is_empty() {
            true => Path::new("."),
            false => &self.base,
        };
        let unreadable = |source| SourceError::Read {
            path: full.clone(),
            source,
        };
        let base = tokio::fs::canonicalize(base).await.map_err(unreadable)?;
        let file = tokio::fs::canonicalize(&full).await.map_err(unreadable)?;
        if !file.starts_with(&base) {
            return Err(outside());
        }
        Ok(file)
    }

    /// That `file`, opened from `full`, is what [`confined`](Self::confined) found:
    /// a link swapped in between that check and the open led it elsewhere. Asked of
    /// the file already open, so what is swapped afterwards changes nothing.
    async fn still_confined(
        &self,
        path: &Path,
        full: &Path,
        file: &tokio::fs::File,
    ) -> Result<(), SourceError> {
        let failed = |source| SourceError::Read {
            path: full.to_path_buf(),
            source,
        };
        let outside = || SourceError::OutsideBase {
            path: path.to_path_buf(),
        };
        if self.confined(path).await? != full {
            return Err(outside());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata().await.map_err(failed)?;
            let there = tokio::fs::symlink_metadata(full).await.map_err(failed)?;
            if (opened.dev(), opened.ino()) != (there.dev(), there.ino()) {
                return Err(outside());
            }
        }
        #[cfg(not(unix))]
        let _ = (file, failed);
        Ok(())
    }

    /// The cache file for `digest`.
    fn cache_path(&self, digest: &Digest) -> Option<PathBuf> {
        Some(
            self.cache
                .as_ref()?
                .join("sha256")
                .join(format!("{}.wasm", digest.hex())),
        )
    }

    /// Stores pulled bytes in the cache, under their digest.
    ///
    /// The write runs on a blocking task of its own, which finishes (file
    /// renamed into place, or removed) even when this resolve is dropped
    /// part way, as when another source fails or the caller times out.
    async fn store(&self, resolved: &Fetched) {
        let Some(path) = self.cache_path(&resolved.digest) else {
            return;
        };
        let bytes = resolved.bytes.clone();
        // Written beside its final name and renamed, so a reader never
        // sees half a file. The partial file's name is this write's own:
        // another write of the same digest (in this process or another)
        // never touches it.
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let partial = path.with_extension(format!(
            "{}-{}.partial",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let write = move || {
            if let Some(dir) = path.parent()
                && std::fs::create_dir_all(dir).is_err()
            {
                return;
            }
            let written = std::fs::write(&partial, &bytes).is_ok()
                && std::fs::rename(&partial, &path).is_ok();
            if !written {
                let _ = std::fs::remove_file(&partial);
            }
        };
        let _ = tokio::task::spawn_blocking(write).await;
    }

    /// Pulls the one layer of the kind wanted of an OCI artifact, with its
    /// digest; `None` when it is over the size limit (refused by its
    /// declared size, or cut off while it arrives).
    async fn pull(
        &self,
        reference: &str,
        parsed: &Reference,
        kind: Layer,
    ) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
        let failed = |e: oci_client::errors::OciDistributionError| SourceError::Pull {
            reference: reference.to_string(),
            message: format!("{e}"),
        };
        // The manifest first: an artifact may carry other layers (a
        // signature, say) beside the one wanted.
        let (manifest, _) = self
            .client
            .pull_image_manifest(parsed, self.auth_for(parsed))
            .await
            .map_err(failed)?;
        let mut layers = manifest
            .layers
            .iter()
            .filter(|layer| kind.accepts(&layer.media_type));
        let layer = match (layers.next(), layers.count()) {
            (Some(layer), 0) => layer,
            (first, more) => {
                return Err(SourceError::Layers {
                    reference: reference.to_string(),
                    what: kind.what(),
                    found: usize::from(first.is_some()) + more,
                });
            }
        };
        if u64::try_from(layer.size).is_ok_and(|size| size > self.max_bytes) {
            return Ok(None);
        }
        let mut out = Capped::new(self.max_bytes);
        match self.client.pull_blob(parsed, layer, &mut out).await {
            Ok(()) => Ok(Some(out.finish())),
            Err(_) if out.over => Ok(None),
            Err(e) => Err(failed(e)),
        }
    }
}

impl Resolver {
    /// The credentials for the registry `reference` names.
    fn auth_for(&self, reference: &Reference) -> &RegistryAuth {
        static ANONYMOUS: RegistryAuth = RegistryAuth::Anonymous;
        self.auth
            .get(reference.resolve_registry())
            .or_else(|| self.auth.get(reference.registry()))
            .unwrap_or(&ANONYMOUS)
    }
}

/// A buffer that refuses to grow past `limit` bytes, hashing what it
/// takes as it arrives (so no separate pass over the bytes stalls the
/// other fetches).
struct Capped {
    bytes: Vec<u8>,
    hash: Sha256,
    limit: u64,
    /// Whether a write would have gone past the limit.
    over: bool,
}

impl Capped {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            hash: Sha256::new(),
            limit,
            over: false,
        }
    }

    /// Takes `buf`, unless that would go past the limit.
    fn push(&mut self, buf: &[u8]) -> bool {
        if (self.bytes.len() + buf.len()) as u64 > self.limit {
            self.over = true;
            return false;
        }
        self.bytes.extend_from_slice(buf);
        self.hash.update(buf);
        true
    }

    /// The bytes taken, and their digest.
    fn finish(self) -> (Vec<u8>, Digest) {
        (self.bytes, Digest(hex::encode(self.hash.finalize())))
    }
}

impl tokio::io::AsyncWrite for Capped {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if !self.get_mut().push(buf) {
            return std::task::Poll::Ready(Err(std::io::Error::other("over the size limit")));
        }
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A registry client speaking `protocol`.
fn client(protocol: ClientProtocol) -> Client {
    Client::new(ClientConfig {
        protocol,
        ..ClientConfig::default()
    })
}

/// The file at `path` and its digest, or `None` when it is over `limit`
/// bytes.
async fn read(path: &Path, limit: u64) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
    read_file(open(path).await?, path, limit).await
}

/// `path`, which must be a regular file: without a writer, opening a FIFO would
/// wait forever, and neither the size limit nor a digest would get to look at it.
async fn open(path: &Path) -> Result<tokio::fs::File, SourceError> {
    let failed = |source| SourceError::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).await.map_err(failed)?;
    if !file.metadata().await.map_err(failed)?.is_file() {
        return Err(failed(std::io::Error::other("not a regular file")));
    }
    Ok(file)
}

/// The rest of `file`, read from `path`, and its digest, or `None` when it is over
/// `limit` bytes.
async fn read_file(
    mut file: tokio::fs::File,
    path: &Path,
    limit: u64,
) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
    use tokio::io::AsyncReadExt;
    let failed = |source| SourceError::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut out = Capped::new(limit);
    let mut chunk = vec![0; 64 << 10];
    loop {
        // Checked as it is read, so a file that grows is still cut off.
        match file.read(&mut chunk).await.map_err(failed)? {
            0 => return Ok(Some(out.finish())),
            n if !out.push(&chunk[..n]) => return Ok(None),
            _ => {}
        }
    }
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
        let file: Location = "ghcr.io/demo/sensor.wasm".parse().unwrap();
        assert_eq!(file, Location::File("ghcr.io/demo/sensor.wasm".into()));
        assert_eq!(oci.to_string(), "oci://ghcr.io/demo/sensor:0.1.0");
        assert!("".parse::<Location>().is_err());
    }

    #[test]
    fn a_manifest_names_paths_relative_to_itself() {
        let manifest: Location = "guests/adder.node.json".parse().unwrap();
        let joined = manifest.join(&"out/adder.wasm".parse().unwrap()).unwrap();
        assert_eq!(joined, Location::File("guests/out/adder.wasm".into()));
        let pulled: Location = "oci://ghcr.io/demo/adder:1".parse().unwrap();
        assert!(matches!(
            pulled.join(&"adder.wasm".parse().unwrap()),
            Err(SourceError::FileFromOci { .. })
        ));
        assert_eq!(
            pulled
                .join(&"oci://ghcr.io/demo/adder-wasm:1".parse().unwrap())
                .unwrap(),
            Location::Oci("ghcr.io/demo/adder-wasm:1".into())
        );
    }

    #[test]
    fn node_manifests_read_as_written() {
        let manifest: NodeManifest = serde_json::from_str(
            r#"{
                "wit": "demo:graph@0.1.0",
                "wit_source": "wit",
                "worlds": ["demo:graph/sensor@0.1.0"],
                "component": "oci://ghcr.io/demo/sensor:0.1.0",
                "viewers": { "demo:graph/sensor@0.1.0": { "frames": "image" } }
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.wit.to_string(), "demo:graph@0.1.0");
        assert_eq!(
            manifest.component,
            Location::Oci("ghcr.io/demo/sensor:0.1.0".into())
        );
        let back: NodeManifest =
            serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
        assert_eq!(back, manifest);
        let misspelled =
            r#"{"wit":"d:g@0.1.0","wit_source":"w","worlds":[],"component":"c","viewer":{}}"#;
        assert!(serde_json::from_str::<NodeManifest>(misspelled).is_err());
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
