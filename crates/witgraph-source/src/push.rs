//! Packing a node from its files into an OCI node artifact, and pushing it.

use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;
use oci_client::Reference;
use serde_json::json;
use witgraph_ir::ComponentName;

use crate::oci::{
    Descriptor, IMAGE_INDEX, IMAGE_MANIFEST, ImageIndex, ImageManifest, NODE_ARTIFACT_TYPE,
    NODE_CONFIG_MEDIA_TYPE, PROFILE_ANNOTATION, Repository, WASM_CONFIG_MEDIA_TYPE,
    WASM_LAYER_MEDIA_TYPE,
};
use crate::{Digest, Location, Node, Profile, Resolver, SourceError};

/// When every config pushed says it was made: a fixed time, so a node pushed
/// again from the same files is the same artifact, with the same digest.
const CREATED: &str = "1970-01-01T00:00:00Z";

/// What [`Resolver::push`] pushed.
#[derive(Debug, Clone)]
pub struct Pushed {
    /// The node artifact, pinned: `registry/repository@sha256:…`.
    pub reference: String,
    /// Its digest.
    pub digest: Digest,
    /// Each build's manifest digest, and its profile (none for the one
    /// build).
    pub builds: Vec<(Option<Profile>, Digest)>,
}

impl Resolver {
    /// Packs the node whose manifest is at `manifest` (relative to the base)
    /// into a node artifact, and pushes it as `target`
    /// (`oci://registry/repository:tag`): its builds as Wasm OCI artifacts,
    /// an index of them, and the node manifest, tagged.
    ///
    /// It is checked first as a resolve checks it: every build targets every
    /// world it lists.
    pub async fn push(&self, manifest: &Path, target: &str) -> Result<Pushed, SourceError> {
        let Ok(Location::Oci(reference)) = target.parse::<Location>() else {
            return Err(SourceError::Reference {
                reference: target.to_string(),
                message: "a node is pushed to `oci://registry/repository:tag`".into(),
            });
        };
        let repository = Repository::parse(&reference)?;
        if repository.parsed.digest().is_some() || repository.parsed.tag().is_none() {
            return Err(SourceError::Reference {
                reference: reference.clone(),
                message: "a node is pushed under a tag, not a digest".into(),
            });
        }
        let (read, builds) = self.local_builds(manifest).await?;
        let description = read.manifest.description();
        let name =
            ComponentName::try_from("pushed".to_string()).map_err(|e| SourceError::Manifest {
                location: Location::File(manifest.to_path_buf()),
                message: e.to_string(),
            })?;
        for (profile, bytes, digest) in &builds {
            crate::describe(
                &name,
                &Location::File(manifest.to_path_buf()),
                Node {
                    description: description.clone(),
                    wit: read.wit.clone(),
                    wit_location: Location::File(read.wit_at.clone()),
                    profile: *profile,
                    bytes: bytes.clone(),
                    digest: digest.clone(),
                    artifact: None,
                    offline: false,
                },
            )?;
        }

        // Authenticated for pushing, once: every request below presents it.
        self.client
            .auth(
                &repository.parsed,
                self.auth_for(&repository.parsed),
                oci_client::RegistryOperation::Push,
            )
            .await
            .map_err(|e| pushed(&repository, e))?;
        // What the builds' configs say they are, from the WIT: what each
        // world imports and exports, as wkg says it.
        let (imports, exports) = interface_names(&read.wit, &description.worlds);
        let target_world = match description.worlds.as_slice() {
            [world] => Some(world.to_string()),
            _ => None,
        };
        let mut entries = Vec::new();
        let mut pushed_builds = Vec::new();
        for (profile, bytes, digest) in &builds {
            let config = json_bytes(
                &repository,
                &json!({
                    "created": CREATED,
                    "architecture": "wasm",
                    "os": "wasip2",
                    "layerDigests": [digest.to_string()],
                    "component": { "imports": imports, "exports": exports, "target": target_world },
                }),
            )?;
            let config = self.push_blob(&repository, config.into()).await?;
            let layer = self.push_blob(&repository, bytes.clone()).await?;
            let manifest = ImageManifest {
                schema_version: 2,
                media_type: Some(IMAGE_MANIFEST.into()),
                artifact_type: None,
                config: Descriptor::new(WASM_CONFIG_MEDIA_TYPE, &config.0, config.1),
                layers: vec![Descriptor::new(WASM_LAYER_MEDIA_TYPE, &layer.0, layer.1)],
                subject: None,
            };
            let (digest, size) = self
                .push_manifest(&repository, &manifest, IMAGE_MANIFEST, None)
                .await?;
            let mut entry = Descriptor::new(IMAGE_MANIFEST, &digest, size);
            if let Some(profile) = profile {
                entry.annotations =
                    BTreeMap::from([(PROFILE_ANNOTATION.to_string(), profile.to_string())]);
            }
            entries.push(entry);
            pushed_builds.push((*profile, digest));
        }
        let index = ImageIndex {
            schema_version: 2,
            media_type: Some(IMAGE_INDEX.into()),
            manifests: entries,
        };
        let (index_digest, index_size) = self
            .push_manifest(&repository, &index, IMAGE_INDEX, None)
            .await?;

        let wit = witgraph_wit::load::encode(&read.wit).map_err(|e| SourceError::Wit {
            location: Location::File(read.wit_at.clone()),
            message: e.to_string(),
        })?;
        let wit = self.push_blob(&repository, wit.into()).await?;
        let config = json_bytes(&repository, &description)?;
        let config = self.push_blob(&repository, config.into()).await?;
        let node = ImageManifest {
            schema_version: 2,
            media_type: Some(IMAGE_MANIFEST.into()),
            artifact_type: Some(NODE_ARTIFACT_TYPE.into()),
            config: Descriptor::new(NODE_CONFIG_MEDIA_TYPE, &config.0, config.1),
            layers: vec![Descriptor::new(WASM_LAYER_MEDIA_TYPE, &wit.0, wit.1)],
            subject: Some(Descriptor::new(IMAGE_INDEX, &index_digest, index_size)),
        };
        let (digest, _) = self
            .push_manifest(&repository, &node, IMAGE_MANIFEST, Some(&repository.parsed))
            .await?;
        Ok(Pushed {
            reference: format!(
                "{}/{}@{digest}",
                repository.parsed.registry(),
                repository.parsed.repository()
            ),
            digest,
            builds: pushed_builds,
        })
    }

    /// Pushes `bytes` as a blob of `repository`; their digest and size.
    async fn push_blob(
        &self,
        repository: &Repository,
        bytes: Bytes,
    ) -> Result<(Digest, usize), SourceError> {
        let digest = Digest::of(&bytes);
        let size = bytes.len();
        self.client
            .push_blob(&repository.parsed, bytes, &digest.to_string())
            .await
            .map_err(|e| pushed(repository, e))?;
        Ok((digest, size))
    }

    /// Pushes `manifest` to `repository`, under `tag` if given and its
    /// digest otherwise; its digest and size.
    async fn push_manifest(
        &self,
        repository: &Repository,
        manifest: &impl serde::Serialize,
        media_type: &'static str,
        tag: Option<&Reference>,
    ) -> Result<(Digest, usize), SourceError> {
        let bytes = json_bytes(repository, manifest)?;
        let digest = Digest::of(&bytes);
        let size = bytes.len();
        let at = match tag {
            Some(tag) => tag.clone(),
            None => repository.at(&digest),
        };
        self.client
            .push_manifest_raw(&at, bytes, http::HeaderValue::from_static(media_type))
            .await
            .map_err(|e| pushed(repository, e))?;
        Ok((digest, size))
    }
}

/// `value` as JSON.
fn json_bytes(
    repository: &Repository,
    value: &impl serde::Serialize,
) -> Result<Vec<u8>, SourceError> {
    serde_json::to_vec(value).map_err(|e| SourceError::Push {
        reference: repository.reference.clone(),
        message: e.to_string(),
    })
}

fn pushed(repository: &Repository, e: oci_client::errors::OciDistributionError) -> SourceError {
    SourceError::Push {
        reference: repository.reference.clone(),
        message: e.to_string(),
    }
}

/// What `worlds` import and export, by name, as wkg names them.
fn interface_names(
    wit: &witgraph_wit::load::WitSource,
    worlds: &[witgraph_ir::WorldRef],
) -> (Vec<String>, Vec<String>) {
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    for world in worlds {
        let Some(id) = witgraph_wit::lower::find_world(wit, world) else {
            continue;
        };
        let world = &wit.resolve.worlds[id];
        imports.extend(
            world
                .imports
                .keys()
                .map(|key| wit.resolve.name_world_key(key)),
        );
        exports.extend(
            world
                .exports
                .keys()
                .map(|key| wit.resolve.name_world_key(key)),
        );
    }
    for names in [&mut imports, &mut exports] {
        names.sort();
        names.dedup();
    }
    (imports, exports)
}
