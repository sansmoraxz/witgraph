//! A node packed as files: its node manifest, a `*.node.json` naming its WIT
//! and its builds by paths relative to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use witgraph_ir::{NodeDescription, WitRef, WorldRef};
use witgraph_wit::load::WitSource;

use crate::files::is_wasm;
use crate::{Digest, Location, Node, Profile, Resolver, SourceError};

/// A node manifest as written beside its files:
///
/// ```json
/// {
///   "wit": "demo:graph@0.1.0",
///   "wit_source": "wit",
///   "worlds": ["demo:graph/sensor@0.1.0"],
///   "component": { "release": "out/sensor.wasm", "debug": "out/sensor.debug.wasm" },
///   "viewers": { "demo:graph/sensor@0.1.0": { "frames": "image" } }
/// }
/// ```
///
/// Its description (`wit`, `worlds`, `viewers`), and where its files are,
/// relative to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalManifest {
    /// The WIT package its worlds are in.
    pub wit: WitRef,
    /// Where that WIT is: a directory or `.wit` file (with its `deps/`), or
    /// the package encoded, as `wkg build` writes it.
    pub wit_source: PathBuf,
    /// Every world of `wit` it provides.
    pub worlds: Vec<WorldRef>,
    /// Its builds.
    pub component: Builds,
    /// How an editor previews outputs: per world, output port to viewer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub viewers: BTreeMap<WorldRef, BTreeMap<String, String>>,
}

impl LocalManifest {
    /// What it says the component is.
    pub fn description(&self) -> NodeDescription {
        NodeDescription {
            wit: self.wit.clone(),
            worlds: self.worlds.clone(),
            viewers: self.viewers.clone(),
        }
    }
}

/// A component's builds: one, used whatever [`Profile`] is asked for, or one
/// per profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Builds {
    /// The one build.
    One(PathBuf),
    /// A build per profile.
    Profiles(BTreeMap<Profile, PathBuf>),
}

impl Builds {
    /// The build for `profile`, and the profile it is (none for the one
    /// build).
    pub fn get(&self, profile: Profile) -> Option<(Option<Profile>, &Path)> {
        match self {
            Self::One(path) => Some((None, path)),
            Self::Profiles(builds) => builds
                .get(&profile)
                .map(|path| (Some(profile), path.as_path())),
        }
    }

    /// Every build, and the profile each is.
    pub fn all(&self) -> Vec<(Option<Profile>, &Path)> {
        match self {
            Self::One(path) => vec![(None, path)],
            Self::Profiles(builds) => builds
                .iter()
                .map(|(profile, path)| (Some(*profile), path.as_path()))
                .collect(),
        }
    }

    /// The profiles there are builds for.
    pub fn profiles(&self) -> Vec<Profile> {
        match self {
            Self::One(_) => Vec::new(),
            Self::Profiles(builds) => builds.keys().copied().collect(),
        }
    }
}

/// A node manifest read, with its WIT.
pub(crate) struct Read {
    pub(crate) manifest: LocalManifest,
    pub(crate) wit: WitSource,
    /// Where the WIT is, relative to the base.
    pub(crate) wit_at: PathBuf,
    /// The manifest's directory, relative to the base: its paths are
    /// relative to it.
    pub(crate) dir: PathBuf,
}

impl Resolver {
    /// The node whose manifest is at `path` (relative to the base), with its
    /// build for the resolver's profile.
    pub(crate) async fn local(&self, path: &Path) -> Result<Node, SourceError> {
        let read = self.read_local(path).await?;
        let (profile, build) =
            read.manifest
                .component
                .get(self.profile)
                .ok_or_else(|| SourceError::NoBuild {
                    location: Location::File(path.to_path_buf()),
                    profile: self.profile,
                    found: read.manifest.component.profiles(),
                })?;
        let (bytes, digest) = self.read_file(&read.dir.join(build)).await?;
        Ok(Node {
            description: read.manifest.description(),
            wit: read.wit,
            wit_location: Location::File(read.wit_at),
            profile,
            bytes,
            digest,
            artifact: None,
            offline: false,
        })
    }

    /// The node whose manifest is at `path`, with every build.
    pub(crate) async fn local_builds(
        &self,
        path: &Path,
    ) -> Result<(Read, Vec<(Option<Profile>, Bytes, Digest)>), SourceError> {
        let read = self.read_local(path).await?;
        if let Builds::Profiles(builds) = &read.manifest.component
            && builds.is_empty()
        {
            return Err(SourceError::Manifest {
                location: Location::File(path.to_path_buf()),
                message: "it names no build".into(),
            });
        }
        let mut builds = Vec::new();
        for (profile, build) in read.manifest.component.all() {
            let (bytes, digest) = self.read_file(&read.dir.join(build)).await?;
            builds.push((profile, bytes, digest));
        }
        Ok((read, builds))
    }

    /// The node manifest at `path`, and the WIT it names.
    async fn read_local(&self, path: &Path) -> Result<Read, SourceError> {
        let location = Location::File(path.to_path_buf());
        let (bytes, _) = self.read_file(path).await?;
        let manifest: LocalManifest =
            serde_json::from_slice(&bytes).map_err(|e| SourceError::Manifest {
                location: location.clone(),
                message: e.to_string(),
            })?;
        let named = std::iter::once(manifest.wit_source.as_path())
            .chain(manifest.component.all().into_iter().map(|(_, path)| path));
        for named in named {
            if named.to_string_lossy().starts_with(crate::OCI_SCHEME) {
                return Err(SourceError::Manifest {
                    location,
                    message: format!(
                        "it names `{}`: a node manifest names its files, and a node in a \
                         registry is pushed whole",
                        named.display()
                    ),
                });
            }
        }
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let wit_at = dir.join(&manifest.wit_source);
        let wit = self.local_wit(&wit_at).await?;
        Ok(Read {
            manifest,
            wit,
            wit_at,
            dir,
        })
    }

    /// The WIT at `path`: a directory or `.wit` file, or a package encoded in
    /// a wasm file, never a component to read WIT out of.
    async fn local_wit(&self, path: &Path) -> Result<WitSource, SourceError> {
        let failed = |message: String| SourceError::Wit {
            location: Location::File(path.to_path_buf()),
            message,
        };
        let full = self.confined(path).await?;
        let loaded = tokio::task::spawn_blocking(move || {
            if full.is_file() && is_wasm(&full) {
                let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
                return witgraph_wit::load::load_encoded(&bytes).map_err(|e| e.to_string());
            }
            witgraph_wit::load::load_path(full).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| failed(e.to_string()))?;
        loaded.map_err(failed)
    }
}
