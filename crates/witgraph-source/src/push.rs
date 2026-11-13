//! Packing a node from its files into an OCI node artifact, and pushing it.

use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;
use oci_client::Reference;
use serde_json::json;
use witgraph_ir::ComponentName;

use crate::oci::{
    Descriptor, IMAGE_INDEX, IMAGE_MANIFEST, ImageIndex, ImageManifest, NODE_ANNOTATION,
    PROFILE_ANNOTATION, ROLE_ANNOTATION, ROLE_BUILD, ROLE_WIT, Repository, WASM_CONFIG_MEDIA_TYPE,
    WASM_LAYER_MEDIA_TYPE,
};
use crate::{Digest, Location, Node, Profile, Resolver, SourceError};

/// When every config pushed says it was made: a fixed time, so a node pushed
/// again from the same files is the same artifact, with the same digest.
const CREATED: &str = "1970-01-01T00:00:00Z";

/// The `os` every config pushed says: WASI 0.3, whose async component model
/// every node needs (its `run`, streams and futures), whatever it imports.
const OS: &str = "wasip3";

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
        // The WIT package, as `wkg publish` pushes one: what its config says
        // it exports, from the WIT.
        let wit = witgraph_wit::load::encode(&read.wit).map_err(|e| SourceError::Wit {
            location: Location::File(read.wit_at.clone()),
            message: e.to_string(),
        })?;
        let wit = self
            .push_wasm_artifact(&repository, wit.into(), &package_component(&read.wit))
            .await?;
        let mut entries = vec![entry(wit.0, wit.1, ROLE_WIT, None)];

        // The builds, as `wkg oci push` pushes them: what each world imports
        // and exports, from the WIT.
        let (imports, exports) = interface_names(&read.wit, &description.worlds);
        let target_world = match description.worlds.as_slice() {
            [world] => Some(world.to_string()),
            _ => None,
        };
        let component = json!({ "imports": imports, "exports": exports, "target": target_world });
        let mut pushed_builds = Vec::new();
        for (profile, bytes, _) in &builds {
            let (digest, size) = self
                .push_wasm_artifact(&repository, bytes.clone(), &component)
                .await?;
            entries.push(entry(digest.clone(), size, ROLE_BUILD, *profile));
            pushed_builds.push((*profile, digest));
        }

        let index = ImageIndex {
            schema_version: 2,
            media_type: Some(IMAGE_INDEX.into()),
            manifests: entries,
            annotations: BTreeMap::from([(
                NODE_ANNOTATION.to_string(),
                serde_json::to_string(&description).map_err(|e| SourceError::Push {
                    reference: repository.reference.clone(),
                    message: e.to_string(),
                })?,
            )]),
        };
        let (digest, _) = self
            .push_manifest(&repository, &index, IMAGE_INDEX, Some(&repository.parsed))
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

    /// Pushes `bytes` as a Wasm OCI artifact whose config says it is
    /// `component`; its manifest's digest and size.
    async fn push_wasm_artifact(
        &self,
        repository: &Repository,
        bytes: Bytes,
        component: &serde_json::Value,
    ) -> Result<(Digest, usize), SourceError> {
        let config = json_bytes(
            repository,
            &json!({
                "created": CREATED,
                "architecture": "wasm",
                "os": OS,
                "layerDigests": [Digest::of(&bytes).to_string()],
                "component": component,
            }),
        )?;
        let config = self.push_blob(repository, config.into()).await?;
        let layer = self.push_blob(repository, bytes).await?;
        let manifest = ImageManifest {
            schema_version: 2,
            media_type: Some(IMAGE_MANIFEST.into()),
            config: Descriptor::new(WASM_CONFIG_MEDIA_TYPE, &config.0, config.1),
            layers: vec![Descriptor::new(WASM_LAYER_MEDIA_TYPE, &layer.0, layer.1)],
        };
        self.push_manifest(repository, &manifest, IMAGE_MANIFEST, None)
            .await
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

/// An entry of a node's index: the manifest `digest`, in `role`, of
/// `profile` if given.
fn entry(digest: Digest, size: usize, role: &str, profile: Option<Profile>) -> Descriptor {
    let mut entry = Descriptor::new(IMAGE_MANIFEST, &digest, size);
    entry
        .annotations
        .insert(ROLE_ANNOTATION.to_string(), role.to_string());
    if let Some(profile) = profile {
        entry
            .annotations
            .insert(PROFILE_ANNOTATION.to_string(), profile.to_string());
    }
    entry
}

/// What a WIT package's config says it is, as wkg says it: everything its
/// worlds export, the worlds themselves, and its interfaces; no imports.
fn package_component(wit: &witgraph_wit::load::WitSource) -> serde_json::Value {
    let resolve = &wit.resolve;
    let mut exports = Vec::new();
    for &id in wit.packages.first().into_iter() {
        let package = &resolve.packages[id];
        for &world_id in package.worlds.values() {
            let world = &resolve.worlds[world_id];
            exports.extend(world.exports.keys().map(|key| resolve.name_world_key(key)));
            let mut name = format!(
                "{}:{}/{}",
                package.name.namespace, package.name.name, world.name
            );
            if let Some(version) = &package.name.version {
                name.push_str(&format!("@{version}"));
            }
            exports.push(name);
        }
        exports.extend(
            package
                .interfaces
                .values()
                .filter_map(|&id| resolve.id_of(id)),
        );
    }
    exports.sort();
    exports.dedup();
    json!({ "exports": exports, "imports": [], "target": null })
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
