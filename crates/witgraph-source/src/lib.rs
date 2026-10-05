//! Resolving the component bytes a graph runs, from files and OCI
//! registries.
//!
//! A graph names its components by [`ComponentRef`]; where their bytes come
//! from is kept beside it, as a list of [`ComponentSource`]s (plain serde
//! data). A [`Resolver`] turns that list into [`Components`]: the bytes of
//! every component, keyed by its ref, which is what
//! `witgraph_runtime::RuntimeGraph::load` takes.
//!
//! ```json
//! [
//!   { "component": { "package": { "namespace": "demo", "name": "graph", "version": "0.1.0" }, "world": "sensor" },
//!     "oci": "ghcr.io/demo/sensor:0.1.0",
//!     "digest": "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" },
//!   { "component": { "package": { "namespace": "demo", "name": "graph", "version": "0.1.0" }, "world": "alarm" },
//!     "file": "components/alarm.wasm" }
//! ]
//! ```
//!
//! # Digests
//!
//! A source may pin a [`Digest`]: the sha-256 of the component's bytes
//! (`sha256:<hex>`). Resolving checks it, whatever the source, and fails
//! with [`SourceError::DigestMismatch`] on any other bytes. It is the
//! digest of the component itself, the same for a file and for the layer
//! of an OCI artifact, not the digest of an OCI manifest.
//!
//! # Cache
//!
//! With a cache directory ([`Resolver::cache`]), every component pulled
//! from a registry is stored under its digest, and a pinned source whose
//! digest is already there is read from disk without contacting the
//! registry. Cached bytes are checked against their digest like any other.
//!
//! # OCI artifacts
//!
//! An OCI source is an image reference (`registry/repository:tag`, or
//! `@sha256:…` for a manifest digest). The artifact must hold exactly one
//! WebAssembly layer ([`WASM_LAYER_MEDIA_TYPES`]), as `wkg oci push` and
//! `wash push` produce; layers of other media types are ignored.

use std::collections::HashMap;

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
use witgraph_ir::ComponentRef;

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

/// Where a component's bytes are.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    /// A file holding the encoded component. A relative path is resolved
    /// against [`Resolver::base`].
    File(PathBuf),
    /// An OCI image reference (`registry/repository:tag`).
    Oci(String),
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "file `{}`", path.display()),
            Self::Oci(reference) => write!(f, "OCI artifact `{reference}`"),
        }
    }
}

/// Where the bytes of one component come from.
///
/// In JSON, the location is a `file` or an `oci` key beside `component`
/// and `digest`. Any other key is an error, so a misspelled `digest` cannot
/// leave a source silently unpinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSource", into = "RawSource")]
pub struct ComponentSource {
    /// The component, as the graph's component table names it. With a
    /// content hash it supplies that revision only; without, any revision
    /// of the same package, version and world.
    pub component: ComponentRef,
    /// Where its bytes are.
    pub location: Location,
    /// The sha-256 the bytes must have, when pinned.
    pub digest: Option<Digest>,
}

/// A [`ComponentSource`] as JSON has it: `#[serde(flatten)]` cannot deny
/// unknown fields, so the location's keys are spelled out.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    component: ComponentRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oci: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<Digest>,
}

impl TryFrom<RawSource> for ComponentSource {
    type Error = String;

    fn try_from(raw: RawSource) -> Result<Self, Self::Error> {
        let location = match (raw.file, raw.oci) {
            (Some(path), None) => Location::File(path),
            (None, Some(reference)) => Location::Oci(reference),
            (None, None) => return Err("a source needs a `file` or an `oci` location".into()),
            (Some(_), Some(_)) => return Err("a source has one location: `file` or `oci`".into()),
        };
        Ok(Self {
            component: raw.component,
            location,
            digest: raw.digest,
        })
    }
}

impl From<ComponentSource> for RawSource {
    fn from(source: ComponentSource) -> Self {
        let (file, oci) = match source.location {
            Location::File(path) => (Some(path), None),
            Location::Oci(reference) => (None, Some(reference)),
        };
        Self {
            component: source.component,
            file,
            oci,
            digest: source.digest,
        }
    }
}

impl ComponentSource {
    /// `component`'s bytes are the file at `path`.
    pub fn file(component: ComponentRef, path: impl Into<PathBuf>) -> Self {
        Self {
            component,
            location: Location::File(path.into()),
            digest: None,
        }
    }

    /// `component`'s bytes are the OCI artifact `reference`.
    pub fn oci(component: ComponentRef, reference: impl Into<String>) -> Self {
        Self {
            component,
            location: Location::Oci(reference.into()),
            digest: None,
        }
    }

    /// Pins the bytes to `digest`.
    pub fn pinned(mut self, digest: Digest) -> Self {
        self.digest = Some(digest);
        self
    }
}

/// Why component bytes could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Two sources name the same component.
    #[error("component `{component:#}` has more than one source")]
    Duplicate {
        /// The component named twice.
        component: Box<ComponentRef>,
    },
    /// A component file could not be read.
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
    /// An OCI artifact holds no WebAssembly layer, or several. Layers of
    /// other media types (a signature, say) are ignored.
    #[error("`{reference}` holds {found} WebAssembly layers; a component artifact has one")]
    Layers {
        /// The artifact.
        reference: String,
        /// How many WebAssembly layers it holds.
        found: usize,
    },
    /// The bytes are not the ones the source pins.
    #[error("{location} for `{component:#}` has digest {found}, but {expected} is pinned")]
    DigestMismatch {
        /// The component.
        component: Box<ComponentRef>,
        /// Where the bytes came from.
        location: Location,
        /// The pinned digest.
        expected: Digest,
        /// The digest of the bytes found.
        found: Digest,
    },
    /// A file source names a path outside [`Resolver::base`]: absolute,
    /// leaving it through `..`, or leading out of it through a symbolic
    /// link (see [`Resolver::allow_paths_outside_base`]).
    #[error("file source `{}` is outside the base directory", path.display())]
    OutsideBase {
        /// The path as the source names it.
        path: PathBuf,
    },
    /// A component is larger than [`Resolver::max_component_bytes`].
    #[error("{location} for `{component:#}` is over the limit of {limit} bytes")]
    TooLarge {
        /// The component.
        component: Box<ComponentRef>,
        /// Where its bytes are.
        location: Location,
        /// The limit.
        limit: u64,
    },
}

/// The bytes of one resolved component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The encoded component, shared by every source that names it.
    pub bytes: Bytes,
    /// The sha-256 of `bytes`: what to pin to get these bytes again.
    pub digest: Digest,
}

/// The bytes of every component a list of sources names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Components(HashMap<ComponentRef, Resolved>);

impl Components {
    /// The components' bytes by ref: what
    /// `witgraph_runtime::RuntimeGraph::load` takes.
    pub fn wasm(&self) -> HashMap<ComponentRef, &[u8]> {
        self.0
            .iter()
            .map(|(component, resolved)| (component.clone(), &*resolved.bytes))
            .collect()
    }

    /// The bytes and digest resolved for `component`.
    pub fn get(&self, component: &ComponentRef) -> Option<&Resolved> {
        self.0.get(component)
    }

    /// Every resolved component.
    pub fn iter(&self) -> impl Iterator<Item = (&ComponentRef, &Resolved)> {
        self.0.iter()
    }

    /// How many components were resolved.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no component was resolved.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Fetches component bytes from their sources.
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
    /// the path its links resolved to when it was checked, so a link
    /// changed after the check is not followed.
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

    /// Resolves every source, several at once: reads the files, pulls the
    /// artifacts (or reads them from the cache), and checks each pinned
    /// digest. Sources with the same location and pin are fetched once.
    /// Fails if any source cannot be resolved, with the error of the first
    /// in list order (fetches not yet started then are not), and on a
    /// component named twice.
    pub async fn resolve(&self, sources: &[ComponentSource]) -> Result<Components, SourceError> {
        // Before anything is fetched.
        let mut named = std::collections::HashSet::with_capacity(sources.len());
        if let Some(twice) = sources.iter().find(|s| !named.insert(&s.component)) {
            return Err(SourceError::Duplicate {
                component: Box::new(twice.component.clone()),
            });
        }
        // One fetch per location and pin; the sources sharing it share its
        // result (a mismatch names the first of them).
        let mut fetches: Vec<&ComponentSource> = Vec::new();
        let mut fetch_of: HashMap<(&Location, Option<&Digest>), usize> = HashMap::new();
        let which: Vec<usize> = sources
            .iter()
            .map(|source| {
                *fetch_of
                    .entry((&source.location, source.digest.as_ref()))
                    .or_insert_with(|| {
                        fetches.push(source);
                        fetches.len() - 1
                    })
            })
            .collect();
        // At most `FETCHES` at once; the first failure, in list order,
        // ends the rest.
        let fetched: Vec<Resolved> = futures::stream::iter(fetches.iter().map(|s| self.fetch(s)))
            .buffered(FETCHES)
            .try_collect()
            .await?;
        // The bytes are shared (`Bytes`), so every source of a fetch holds
        // the same ones.
        let components = sources
            .iter()
            .zip(which)
            .map(|(source, fetch)| (source.component.clone(), fetched[fetch].clone()))
            .collect();
        Ok(Components(components))
    }

    /// Resolves one source.
    pub async fn fetch(&self, source: &ComponentSource) -> Result<Resolved, SourceError> {
        let check = |(bytes, digest): (Vec<u8>, Digest)| match &source.digest {
            Some(expected) if *expected != digest => Err(SourceError::DigestMismatch {
                component: Box::new(source.component.clone()),
                location: source.location.clone(),
                expected: expected.clone(),
                found: digest,
            }),
            _ => Ok(Resolved {
                bytes: bytes.into(),
                digest,
            }),
        };
        let too_large = || SourceError::TooLarge {
            component: Box::new(source.component.clone()),
            location: source.location.clone(),
            limit: self.max_bytes,
        };
        match &source.location {
            Location::File(path) => {
                let full = self.confined(path).await?;
                let bytes = read(&full, self.max_bytes).await?;
                check(bytes.ok_or_else(too_large)?)
            }
            Location::Oci(reference) => {
                // Parsed first, so a malformed reference fails whether or
                // not its digest is cached.
                let parsed: Reference = reference.parse().map_err(|e| SourceError::Reference {
                    reference: reference.clone(),
                    message: format!("{e}"),
                })?;
                if let Some(resolved) = self.cached(source.digest.as_ref()).await {
                    // Read under its pinned digest, and checked against it.
                    return Ok(resolved);
                }
                let bytes = self.pull(reference, &parsed).await?.ok_or_else(too_large)?;
                // Only bytes that pass the check are cached. Caching is
                // best effort: the bytes are verified and in hand whether
                // or not they could be kept.
                let resolved = check(bytes)?;
                self.store(&resolved).await;
                Ok(resolved)
            }
        }
    }

    /// The path a file source's bytes are read from: `path` under the
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

    /// The cache file for `digest`.
    fn cache_path(&self, digest: &Digest) -> Option<PathBuf> {
        Some(
            self.cache
                .as_ref()?
                .join("sha256")
                .join(format!("{}.wasm", digest.hex())),
        )
    }

    /// The cached bytes for a pinned digest, if they are there and intact.
    async fn cached(&self, digest: Option<&Digest>) -> Option<Resolved> {
        let digest = digest?;
        let (bytes, found) = read(&self.cache_path(digest)?, self.max_bytes)
            .await
            .ok()??;
        // A damaged cache file is pulled again rather than trusted.
        (found == *digest).then(|| Resolved {
            bytes: bytes.into(),
            digest: found,
        })
    }

    /// Stores pulled bytes in the cache, under their digest.
    ///
    /// The write runs on a blocking task of its own, which finishes (file
    /// renamed into place, or removed) even when this resolve is dropped
    /// part way, as when another source fails or the caller times out.
    async fn store(&self, resolved: &Resolved) {
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

    /// Pulls the one WebAssembly layer of an OCI artifact, with its digest;
    /// `None` when it is over the size limit (refused by its declared size,
    /// or cut off while it arrives).
    async fn pull(
        &self,
        reference: &str,
        parsed: &Reference,
    ) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
        let failed = |e: oci_client::errors::OciDistributionError| SourceError::Pull {
            reference: reference.to_string(),
            message: format!("{e}"),
        };
        // The manifest first: an artifact may carry other layers (a
        // signature, say) beside its component.
        let (manifest, _) = self
            .client
            .pull_image_manifest(parsed, self.auth_for(parsed))
            .await
            .map_err(failed)?;
        let mut layers = manifest
            .layers
            .iter()
            .filter(|layer| WASM_LAYER_MEDIA_TYPES.contains(&layer.media_type.as_str()));
        let layer = match (layers.next(), layers.count()) {
            (Some(layer), 0) => layer,
            (first, more) => {
                return Err(SourceError::Layers {
                    reference: reference.to_string(),
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
    use tokio::io::AsyncReadExt;
    let failed = |source| SourceError::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut file = tokio::fs::File::open(path).await.map_err(failed)?;
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
    fn sources_round_trip_as_json() {
        let component: ComponentRef = "demo:graph/sensor@0.1.0".parse().unwrap();
        let sources = vec![
            ComponentSource::oci(component.clone(), "ghcr.io/demo/sensor:0.1.0")
                .pinned(EMPTY.parse().unwrap()),
            ComponentSource::file(component, "components/sensor.wasm"),
        ];
        let json = serde_json::to_value(&sources).unwrap();
        assert_eq!(json[0]["oci"], "ghcr.io/demo/sensor:0.1.0");
        assert_eq!(json[0]["digest"], EMPTY);
        assert_eq!(json[1]["file"], "components/sensor.wasm");
        assert!(json[1].get("digest").is_none());
        let back: Vec<ComponentSource> = serde_json::from_value(json).unwrap();
        assert_eq!(back, sources);
    }

    #[test]
    fn a_source_with_a_malformed_digest_does_not_deserialize() {
        let json = r#"{
            "component": { "package": { "namespace": "demo", "name": "graph" }, "world": "w" },
            "file": "w.wasm",
            "digest": "md5:abc"
        }"#;
        assert!(serde_json::from_str::<ComponentSource>(json).is_err());
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

    #[test]
    fn a_misspelled_or_missing_key_is_an_error() {
        let source = |rest: &str| {
            serde_json::from_str::<ComponentSource>(&format!(
                r#"{{ "component": {{ "package": {{ "namespace": "demo", "name": "graph" }}, "world": "w" }}{rest} }}"#
            ))
        };
        assert!(source(r#", "oci": "ghcr.io/demo/w:1", "digets": "sha256:00""#).is_err());
        assert!(source("").is_err(), "no location");
        assert!(source(r#", "file": "w.wasm", "oci": "ghcr.io/demo/w:1""#).is_err());
        assert!(source(r#", "file": "w.wasm""#).is_ok());
    }
}
