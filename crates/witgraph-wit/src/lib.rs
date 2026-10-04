//! WIT loading and lowering into the witgraph IR.

pub mod hash;
pub mod load;
pub mod lower;
pub mod metadata;
pub mod verify;

pub use witgraph_ir as ir;

use std::path::Path;

use witgraph_ir::ComponentContract;

/// Failure from [`load_components`]: loading the WIT source or lowering its
/// worlds.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Loading or resolving the WIT source failed.
    #[error(transparent)]
    Load(#[from] load::LoadError),
    /// One or more worlds failed to lower.
    #[error(transparent)]
    Lower(#[from] lower::LowerFailures),
    /// A component's embedded WIT does not describe exactly one node
    /// world.
    #[error("expected exactly one node world in the component, found {found}")]
    NotOneNode {
        /// How many node worlds it describes.
        found: usize,
    },
    /// The WIT source has no component world the ref names.
    #[error("the WIT source has no component world `{id}`")]
    NoWorld {
        /// The ref looked for.
        id: String,
    },
    /// The WIT source has the world, but its contract hashes otherwise
    /// than the ref pins: the source changed since the ref was taken.
    #[error("the WIT source's world `{id}` has content hash {found}, not the {expected} pinned")]
    HashMismatch {
        /// The world, without a hash.
        id: String,
        /// The hash the ref pins.
        expected: String,
        /// The hash of the source's contract.
        found: String,
    },
}

/// Load a `.wit` file or directory and lower every witgraph component world
/// found in it, keeping only the contracts. Fails if any component world
/// fails to lower (every failure is reported); see [`load_each`].
pub fn load_components(path: impl AsRef<Path>) -> Result<Vec<ComponentContract>, Error> {
    Ok(load_lowered(path)?
        .into_iter()
        .map(|lowered| lowered.contract)
        .collect())
}

/// Like [`load_components`], but keeps each world's named types alongside
/// its contract (what [`metadata::generate_catalog`] needs).
///
/// Like [`load_components`], it fails if any component world fails to
/// lower, discarding the others; [`load_each`] keeps them.
pub fn load_lowered(path: impl AsRef<Path>) -> Result<Vec<lower::Lowered>, Error> {
    Ok(lower::lower(&load::load_path(path)?)?)
}

/// Load a `.wit` file or directory and lower each component world in it on
/// its own: one result per world, so a world that fails does not discard
/// the others (an editor can still offer them). Fails only if the source
/// itself does not load.
pub fn load_each(
    path: impl AsRef<Path>,
) -> Result<Vec<Result<lower::Lowered, lower::LowerError>>, load::LoadError> {
    Ok(lower::lower_each(&load::load_path(path)?))
}

/// Lower WIT text (one document; `name` is its pseudo-path in errors) and
/// return the contract of the component world `id` names: the contract a
/// catalog lowered from the same source has, content hash included. For a
/// host without a filesystem, to check component bytes against their
/// source ([`verify::verify_component`]).
pub fn contract_from_str(
    name: &str,
    wit: &str,
    id: &witgraph_ir::ComponentRef,
) -> Result<ComponentContract, Error> {
    let unpinned = witgraph_ir::ComponentRef {
        content_hash: None,
        ..id.clone()
    };
    let source = load::load_str(name, wit)?;
    for lowered in lower::lower_matching(&source, |world| world.matches(&unpinned)) {
        match lowered {
            Ok(lowered) if lowered.contract.id.matches(&unpinned) => {
                let found = &lowered.contract.id.content_hash;
                return match (&id.content_hash, found) {
                    (Some(expected), Some(found)) if expected != found => {
                        Err(Error::HashMismatch {
                            id: unpinned.to_string(),
                            expected: expected.clone(),
                            found: found.clone(),
                        })
                    }
                    _ => Ok(lowered.contract),
                };
            }
            // The world asked for, failing: its own error, not "no world".
            Err(failure) if failure.world.matches(&unpinned) => {
                return Err(Error::Lower(lower::LowerFailures {
                    failures: vec![failure],
                }));
            }
            _ => {}
        }
    }
    Err(Error::NoWorld {
        id: format!("{id:#}"),
    })
}

/// Lower the WIT embedded in an encoded component into its contract, so a
/// graph can be compiled from component artifacts alone, without their WIT
/// source. The component must describe exactly one node world
/// ([`Error::NotOneNode`]).
///
/// A component does not record the package and world it was built from
/// (it decodes as `root:component/root`), so the contract takes them from
/// `id`. Its content hash is computed from the bytes' own contract,
/// whatever hash `id` carries.
///
/// The contract is the component's own view: its capabilities are the
/// imports its code uses, which can be fewer than its WIT source declares,
/// so its content hash can differ from the hash of the contract lowered
/// from that source.
///
/// See [`load::load_component`] on untrusted bytes; a panic lowering what
/// it decoded is contained the same way.
pub fn lower_component(
    bytes: &[u8],
    id: &witgraph_ir::ComponentRef,
) -> Result<lower::Lowered, Error> {
    let source = load::load_component(bytes)?;
    let mut lowered = std::panic::catch_unwind(|| lower::lower(&source)).map_err(|_| {
        Error::Load(load::LoadError::Resolve {
            message: "the component's WIT could not be lowered".into(),
        })
    })??;
    match lowered.len() {
        1 => {
            let mut lowered = lowered.remove(0);
            let content_hash = lowered.contract.id.content_hash.take();
            lowered.contract.id = witgraph_ir::ComponentRef {
                content_hash,
                ..id.clone()
            };
            Ok(lowered)
        }
        found => Err(Error::NotOneNode { found }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIT: &str = r#"
        package demo:pair@0.1.0;

        interface clock {
            now: func() -> u64;
        }

        interface node {
            record inputs { x: u32 }
            record outputs { y: u32 }
            run: async func(inputs: inputs) -> outputs;
        }

        world doubler {
            import clock;
            export node;
        }

        world tripler {
            export node;
        }
    "#;

    #[test]
    fn a_world_is_found_by_its_ref() {
        let lowered = lower::lower(&load::load_str("pair.wit", WIT).unwrap()).unwrap();
        let catalog = |world: &str| {
            lowered
                .iter()
                .find(|l| l.contract.id.world == world)
                .map(|l| l.contract.clone())
                .unwrap()
        };
        for world in ["doubler", "tripler"] {
            let expected = catalog(world);
            let unpinned: witgraph_ir::ComponentRef =
                format!("demo:pair/{world}@0.1.0").parse().unwrap();
            let found = contract_from_str("pair.wit", WIT, &unpinned).unwrap();
            assert_eq!(found, expected, "the catalog's contract, hash included");
            assert_eq!(
                contract_from_str("pair.wit", WIT, &expected.id).unwrap(),
                expected
            );
        }
        let broken = format!("{WIT}\nworld broken {{ export node: func(); }}");
        let id: witgraph_ir::ComponentRef = "demo:pair/broken@0.1.0".parse().unwrap();
        let found = contract_from_str("pair.wit", &broken, &id);
        assert!(
            matches!(found, Err(Error::Lower(_))),
            "a world that fails to lower reports why: {found:?}"
        );
        let stale: witgraph_ir::ComponentRef =
            format!("demo:pair/doubler@0.1.0#{}", "ab".repeat(32))
                .parse()
                .unwrap();
        assert!(matches!(
            contract_from_str("pair.wit", WIT, &stale),
            Err(Error::HashMismatch { .. })
        ));
        let other: witgraph_ir::ComponentRef = "demo:pair/doubler@0.2.0".parse().unwrap();
        assert!(matches!(
            contract_from_str("pair.wit", WIT, &other),
            Err(Error::NoWorld { .. })
        ));
    }
}
