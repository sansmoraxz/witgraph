//! A node packed as an OCI artifact, and pulling one.
//!
//! A node is an OCI image index of Wasm OCI artifacts, its description in
//! the index's annotations:
//!
//! ```text
//! image index   annotation  io.github.sansmoraxz.witgraph.node = {"wit":…,"worlds":…,"viewers":…}
//!   ├─ Wasm OCI artifact   role = wit                      its WIT package, as `wkg publish` pushes one
//!   ├─ Wasm OCI artifact   role = build, profile = release  a build, as `wkg oci push` pushes one
//!   └─ Wasm OCI artifact   role = build, profile = debug
//! ```
//!
//! Every media type is a standard one: an OCI index and manifests, and the
//! CNCF Wasm OCI Artifact spec's config (`application/vnd.wasm.config.v0+json`)
//! and layer (`application/wasm`). What witgraph adds is in annotations, which
//! registries keep as they are given; a registry that checks media types (as
//! GitLab's does) takes a node as any other artifact. A node of one build may
//! leave it without a profile: that build is used whatever profile is asked
//! for.
//!
//! Everything below the index is named by digest: once the index is known,
//! the rest is read from the cache if it is there, and checked against its
//! digest either way.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use oci_client::Reference;
use oci_client::errors::OciDistributionError;
use serde::{Deserialize, Serialize};
use witgraph_ir::NodeDescription;

use crate::files::{Capped, read};
use crate::{Digest, Location, Node, Profile, Resolver, SourceError};

/// The annotation on a node's index holding its description, as JSON: what
/// makes an index a node.
pub const NODE_ANNOTATION: &str = "io.github.sansmoraxz.witgraph.node";

/// The annotation on an entry of a node's index saying what it is:
/// [`ROLE_WIT`] or [`ROLE_BUILD`].
pub const ROLE_ANNOTATION: &str = "io.github.sansmoraxz.witgraph.role";

/// The role of a node's WIT package.
pub const ROLE_WIT: &str = "wit";

/// The role of a node's build.
pub const ROLE_BUILD: &str = "build";

/// The annotation naming a build's [`Profile`].
pub const PROFILE_ANNOTATION: &str = "io.github.sansmoraxz.witgraph.profile";

/// The media type of a WebAssembly layer, in the CNCF Wasm OCI Artifact
/// spec: a component's, or an encoded WIT package's.
pub const WASM_LAYER_MEDIA_TYPE: &str = "application/wasm";

/// The media type of a Wasm OCI artifact's config, in the CNCF Wasm OCI
/// Artifact spec.
pub const WASM_CONFIG_MEDIA_TYPE: &str = "application/vnd.wasm.config.v0+json";

/// An OCI image manifest.
pub(crate) const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// An OCI image index.
pub(crate) const IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// The most bytes a manifest or index may take.
const MAX_MANIFEST_BYTES: usize = 4 << 20;

/// A content descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Descriptor {
    pub(crate) media_type: String,
    pub(crate) digest: String,
    pub(crate) size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) annotations: BTreeMap<String, String>,
}

impl Descriptor {
    pub(crate) fn new(media_type: &str, digest: &Digest, size: usize) -> Self {
        Self {
            media_type: media_type.to_string(),
            digest: digest.to_string(),
            size: size as u64,
            annotations: BTreeMap::new(),
        }
    }
}

/// An OCI image manifest.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageManifest {
    pub(crate) schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media_type: Option<String>,
    pub(crate) config: Descriptor,
    #[serde(default)]
    pub(crate) layers: Vec<Descriptor>,
}

/// What a manifest or index says it is.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaType {
    #[serde(default)]
    media_type: Option<String>,
}

/// An OCI image index.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageIndex {
    pub(crate) schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media_type: Option<String>,
    pub(crate) manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) annotations: BTreeMap<String, String>,
}

/// A parsed reference, and its registry and repository, to name content in
/// it by digest.
pub(crate) struct Repository {
    pub(crate) reference: String,
    pub(crate) parsed: Reference,
}

impl Repository {
    pub(crate) fn parse(reference: &str) -> Result<Self, SourceError> {
        let parsed: Reference = reference.parse().map_err(|e| SourceError::Reference {
            reference: reference.to_string(),
            message: format!("{e}"),
        })?;
        Ok(Self {
            reference: reference.to_string(),
            parsed,
        })
    }

    /// `digest` in this repository.
    pub(crate) fn at(&self, digest: &Digest) -> Reference {
        Reference::with_digest(
            self.parsed.registry().to_string(),
            self.parsed.repository().to_string(),
            digest.to_string(),
        )
    }

    /// `digest` in this repository, as a location.
    pub(crate) fn location(&self, digest: &Digest) -> Location {
        Location::Oci(format!(
            "{}/{}@{digest}",
            self.parsed.registry(),
            self.parsed.repository()
        ))
    }

    /// Why an artifact here is not what it should be.
    pub(crate) fn bad(&self, message: impl Into<String>) -> SourceError {
        SourceError::Artifact {
            reference: self.reference.clone(),
            message: message.into(),
        }
    }

    /// A registry client's error, for this reference.
    pub(crate) fn failed(&self, e: OciDistributionError) -> SourceError {
        failed(&self.reference, e)
    }
}

/// A registry client's error, for `reference`: unreachable if it was never
/// answered.
pub(crate) fn failed(reference: &str, e: OciDistributionError) -> SourceError {
    let (reference, message) = (reference.to_string(), format!("{e}"));
    match &e {
        OciDistributionError::RequestError(e) if e.is_connect() || e.is_timeout() => {
            SourceError::Unreachable { reference, message }
        }
        _ => SourceError::Pull { reference, message },
    }
}

/// What content is pulled as.
enum Content<'a> {
    /// A blob, as its descriptor says.
    Blob(&'a Descriptor),
    /// A manifest of one of these media types.
    Manifest(&'a [&'a str]),
}

impl Resolver {
    /// The node `reference` names, with its build for the resolver's
    /// profile.
    pub(crate) async fn pull_node(&self, reference: &str) -> Result<Node, SourceError> {
        let repository = Repository::parse(reference)?;
        let (bytes, node_digest, offline) = self.node_index(&repository).await?;
        // A manifest (a component, say) is no node, nor an index without a
        // description.
        let not_a_node = || SourceError::NotANode {
            reference: reference.to_string(),
        };
        let kind: MediaType = serde_json::from_slice(&bytes).map_err(|_| not_a_node())?;
        if kind.media_type.as_deref() != Some(IMAGE_INDEX) {
            return Err(not_a_node());
        }
        let index: ImageIndex = serde_json::from_slice(&bytes)
            .map_err(|e| repository.bad(format!("its index does not parse: {e}")))?;
        let description = index
            .annotations
            .get(NODE_ANNOTATION)
            .ok_or_else(not_a_node)?;
        let description: NodeDescription = serde_json::from_str(description)
            .map_err(|e| repository.bad(format!("its description does not parse: {e}")))?;

        let mut wit = None;
        let mut builds = Vec::new();
        for entry in index.manifests {
            match entry.annotations.get(ROLE_ANNOTATION).map(String::as_str) {
                Some(ROLE_WIT) if wit.is_none() => wit = Some(entry),
                Some(ROLE_WIT) => return Err(repository.bad("it holds two WIT packages")),
                Some(ROLE_BUILD) => builds.push(entry),
                Some(other) => return Err(repository.bad(format!("an entry's role is `{other}`"))),
                // Something else beside the node (a signature, say).
                None => {}
            }
        }
        let wit_entry = wit.ok_or_else(|| repository.bad("it holds no WIT package"))?;
        let wit_bytes = self
            .wasm_artifact(&repository, &wit_entry, "its WIT")
            .await?;
        let wit_location = repository.location(&digest_of(&repository, &wit_entry)?);
        let wit = witgraph_wit::load::load_encoded(&wit_bytes).map_err(|e| SourceError::Wit {
            location: wit_location.clone(),
            message: e.to_string(),
        })?;

        let (profile, build) = self.pick(&repository, builds)?;
        let bytes = self.wasm_artifact(&repository, &build, "a build").await?;
        Ok(Node {
            description,
            wit,
            wit_location,
            profile,
            digest: Digest::of(&bytes),
            bytes,
            artifact: Some(node_digest),
            offline,
        })
    }

    /// The one layer of the Wasm OCI artifact `entry` names.
    async fn wasm_artifact(
        &self,
        repository: &Repository,
        entry: &Descriptor,
        what: &str,
    ) -> Result<Bytes, SourceError> {
        let manifest = self
            .content(repository, entry, Content::Manifest(&[IMAGE_MANIFEST]))
            .await?;
        let manifest: ImageManifest = serde_json::from_slice(&manifest)
            .map_err(|e| repository.bad(format!("{what}'s manifest does not parse: {e}")))?;
        if manifest.config.media_type != WASM_CONFIG_MEDIA_TYPE {
            return Err(repository.bad(format!(
                "{what} is not a Wasm OCI artifact: its config is `{}`",
                manifest.config.media_type
            )));
        }
        let layer = one_wasm_layer(repository, &manifest.layers, what)?;
        self.content(repository, layer, Content::Blob(layer)).await
    }

    /// The build of `builds` for the resolver's profile, and its profile:
    /// none for a node's one build left without one.
    fn pick(
        &self,
        repository: &Repository,
        builds: Vec<Descriptor>,
    ) -> Result<(Option<Profile>, Descriptor), SourceError> {
        let one = builds.len() == 1;
        let mut found: Vec<Profile> = Vec::new();
        let mut chosen = None;
        for build in builds {
            let Some(profile) = build.annotations.get(PROFILE_ANNOTATION) else {
                // The one build, whatever the profile.
                if one {
                    return Ok((None, build));
                }
                return Err(
                    repository.bad("it holds several builds, not each annotated with its profile")
                );
            };
            let profile: Profile = profile
                .parse()
                .map_err(|_| repository.bad(format!("a build's profile is `{profile}`")))?;
            if found.contains(&profile) {
                return Err(repository.bad(format!("it holds two {profile} builds")));
            }
            found.push(profile);
            if profile == self.profile {
                chosen = Some(build);
            }
        }
        if found.is_empty() {
            return Err(repository.bad("it holds no build"));
        }
        found.sort();
        let build = chosen.ok_or_else(|| SourceError::NoBuild {
            location: Location::Oci(repository.reference.clone()),
            profile: self.profile,
            found,
        })?;
        Ok((Some(self.profile), build))
    }

    /// The node index `repository` names, its digest, and whether it is
    /// what a tag held when last pulled, as its registry could not be
    /// reached.
    async fn node_index(
        &self,
        repository: &Repository,
    ) -> Result<(Bytes, Digest, bool), SourceError> {
        if let Some(digest) = repository.parsed.digest() {
            let digest: Digest = digest
                .parse()
                .map_err(|_| repository.bad(format!("`{digest}` is not a sha256 digest")))?;
            let bytes = self
                .manifest(repository, &digest, &[IMAGE_INDEX, IMAGE_MANIFEST])
                .await?;
            return Ok((bytes, digest, false));
        }
        match self
            .client
            .pull_manifest_raw(
                &repository.parsed,
                self.auth_for(&repository.parsed),
                &[IMAGE_INDEX, IMAGE_MANIFEST],
            )
            .await
        {
            Ok((bytes, _)) => {
                if bytes.len() > MAX_MANIFEST_BYTES {
                    return Err(repository.bad("its index is too large"));
                }
                let digest = Digest::of(&bytes);
                self.store(&digest, bytes.clone()).await;
                self.store_reference(&repository.reference, &digest).await;
                Ok((bytes, digest, false))
            }
            Err(e) => match repository.failed(e) {
                // What the tag held when last pulled, if that is cached.
                error @ SourceError::Unreachable { .. } => {
                    match self.cached_reference(&repository.reference).await {
                        Some((bytes, digest)) => Ok((bytes, digest, true)),
                        None => Err(error),
                    }
                }
                error => Err(error),
            },
        }
    }

    /// The manifest `digest` names in `repository`.
    async fn manifest(
        &self,
        repository: &Repository,
        digest: &Digest,
        media_types: &[&str],
    ) -> Result<Bytes, SourceError> {
        let descriptor = Descriptor::new(media_types[0], digest, 0);
        self.content(repository, &descriptor, Content::Manifest(media_types))
            .await
    }

    /// The content `descriptor` names in `repository`: from the cache, or
    /// pulled and cached. Checked against its digest either way.
    async fn content(
        &self,
        repository: &Repository,
        descriptor: &Descriptor,
        how: Content<'_>,
    ) -> Result<Bytes, SourceError> {
        let digest = digest_of(repository, descriptor)?;
        if let Some(bytes) = self.cached(&digest).await {
            return Ok(bytes);
        }
        let location = repository.location(&digest);
        let bytes: Bytes = match how {
            Content::Blob(descriptor) => {
                if descriptor.size > self.max_bytes {
                    return Err(SourceError::TooLarge {
                        location,
                        limit: self.max_bytes,
                    });
                }
                let mut out = Capped::new(self.max_bytes);
                let layer = oci_client::manifest::OciDescriptor {
                    media_type: descriptor.media_type.clone(),
                    digest: descriptor.digest.clone(),
                    size: descriptor.size as i64,
                    ..Default::default()
                };
                match self
                    .client
                    .pull_blob(&repository.at(&digest), &layer, &mut out)
                    .await
                {
                    Ok(()) => {}
                    Err(_) if out.over => {
                        return Err(SourceError::TooLarge {
                            location,
                            limit: self.max_bytes,
                        });
                    }
                    Err(e) => return Err(repository.failed(e)),
                }
                out.finish().0.into()
            }
            Content::Manifest(media_types) => {
                let (bytes, _) = self
                    .client
                    .pull_manifest_raw(
                        &repository.at(&digest),
                        self.auth_for(&repository.parsed),
                        media_types,
                    )
                    .await
                    .map_err(|e| repository.failed(e))?;
                if bytes.len() > MAX_MANIFEST_BYTES {
                    return Err(repository.bad(format!("`{digest}` is too large")));
                }
                bytes
            }
        };
        if Digest::of(&bytes) != digest {
            return Err(repository.bad(format!("what `{digest}` names is not what it holds")));
        }
        self.store(&digest, bytes.clone()).await;
        Ok(bytes)
    }

    /// The content cached for `digest`, if it is there and intact.
    pub(crate) async fn cached(&self, digest: &Digest) -> Option<Bytes> {
        let limit = self.max_bytes.max(MAX_MANIFEST_BYTES as u64);
        let (bytes, found) = read(&self.cache_path(digest)?, limit).await.ok()??;
        // A damaged cache file is pulled again rather than trusted.
        (found == *digest).then(|| bytes.into())
    }

    /// What a reference held when it was last pulled, from the cache.
    async fn cached_reference(&self, reference: &str) -> Option<(Bytes, Digest)> {
        let text = tokio::fs::read_to_string(self.reference_path(reference)?)
            .await
            .ok()?;
        let digest: Digest = text.trim().parse().ok()?;
        Some((self.cached(&digest).await?, digest))
    }

    /// Notes in the cache what a reference held.
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
    fn reference_path(&self, reference: &str) -> Option<std::path::PathBuf> {
        Some(
            self.cache
                .as_ref()?
                .join("references")
                .join(Digest::of(reference.as_bytes()).hex()),
        )
    }

    /// The cache file for `digest`.
    fn cache_path(&self, digest: &Digest) -> Option<std::path::PathBuf> {
        Some(self.cache.as_ref()?.join("sha256").join(digest.hex()))
    }

    /// Stores content in the cache, under its digest.
    ///
    /// The write runs on a blocking task of its own, which finishes (file
    /// renamed into place, or removed) even when this resolve is dropped
    /// part way, as when another source fails or the caller times out.
    /// Caching is best effort: the content is in hand whether or not it
    /// could be kept.
    pub(crate) async fn store(&self, digest: &Digest, bytes: Bytes) {
        let Some(path) = self.cache_path(digest) else {
            return;
        };
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
}

/// The one `application/wasm` layer of `layers`.
fn one_wasm_layer<'a>(
    repository: &Repository,
    layers: &'a [Descriptor],
    what: &str,
) -> Result<&'a Descriptor, SourceError> {
    let mut wasm = layers
        .iter()
        .filter(|layer| layer.media_type == WASM_LAYER_MEDIA_TYPE);
    match (wasm.next(), wasm.next()) {
        (Some(layer), None) => Ok(layer),
        (None, _) => Err(repository.bad(format!("{what} has no `{WASM_LAYER_MEDIA_TYPE}` layer"))),
        (Some(_), Some(_)) => Err(repository.bad(format!(
            "{what} has several `{WASM_LAYER_MEDIA_TYPE}` layers"
        ))),
    }
}

/// `descriptor`'s digest, which must be sha256.
fn digest_of(repository: &Repository, descriptor: &Descriptor) -> Result<Digest, SourceError> {
    descriptor
        .digest
        .parse()
        .map_err(|_| repository.bad(format!("`{}` is not a sha256 digest", descriptor.digest)))
}
