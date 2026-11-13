//! WIT loading and lowering into the witgraph IR.
//!
//! A node's contract is its WIT world's, as its node manifest names it:
//! lowered from WIT (a directory, text, or a wasm-encoded package), never
//! read out of the component's bytes. The bytes are only checked to target
//! the world ([`verify`]).

#[cfg(feature = "compose")]
pub mod compose;
pub mod hash;
pub mod hub;
pub mod load;
pub mod lower;
pub mod metadata;
pub mod verify;
pub mod viewer;

pub use witgraph_ir as ir;

use std::path::Path;

use witgraph_ir::{Capability, ComponentContract, ComponentName, ContractId, WorldRef};

/// A component as a runtime builds a graph from it: its bytes, with the WIT
/// its node manifest names.
#[derive(Debug, Clone, Copy)]
pub struct Artifact<'a> {
    /// The encoded component.
    pub bytes: &'a [u8],
    /// The WIT its worlds are in.
    pub wit: &'a load::WitSource,
    /// What every world it provides imports, as its WIT declares it: what
    /// composing it in as a link's provider asks the host for.
    pub capabilities: &'a [Capability],
}

/// Failure from loading WIT and lowering its worlds.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Loading or resolving the WIT source failed.
    #[error(transparent)]
    Load(#[from] load::LoadError),
    /// One or more worlds failed to lower.
    #[error(transparent)]
    Lower(#[from] lower::LowerFailures),
    /// The WIT source has no such world.
    #[error("the WIT has no world `{world}`")]
    NoWorld {
        /// The world looked for.
        world: WorldRef,
    },
    /// The world exports no `node` interface: it is not a node's.
    #[error("world `{world}` exports no `node` interface")]
    NotANode {
        /// The world.
        world: WorldRef,
    },
}

/// Load a `.wit` file or directory (or a wasm-encoded WIT package) and
/// lower every witgraph component world found in it, as `component`
/// provides it, keeping only the contracts. Fails if any component world
/// fails to lower (every failure is reported); see [`load_each`].
pub fn load_components(
    path: impl AsRef<Path>,
    component: &ComponentName,
) -> Result<Vec<ComponentContract>, Error> {
    Ok(load_lowered(path, component)?
        .into_iter()
        .map(|lowered| lowered.contract)
        .collect())
}

/// Like [`load_components`], but keeps each world's named types alongside
/// its contract (what [`metadata::generate_catalog`] needs).
pub fn load_lowered(
    path: impl AsRef<Path>,
    component: &ComponentName,
) -> Result<Vec<lower::Lowered>, Error> {
    Ok(lower::lower(&load::load_path(path)?, component)?)
}

/// Load a `.wit` file or directory and lower each component world in it on
/// its own: one result per world, so a world that fails does not discard
/// the others (an editor can still offer them). Fails only if the source
/// itself does not load.
pub fn load_each(
    path: impl AsRef<Path>,
    component: &ComponentName,
) -> Result<Vec<Result<lower::Lowered, lower::LowerError>>, load::LoadError> {
    Ok(lower::lower_each(&load::load_path(path)?, component))
}

/// Lower the world `id` names, of `source`'s root packages, as `id`'s
/// component provides it.
pub fn lower_world(source: &load::WitSource, id: &ContractId) -> Result<lower::Lowered, Error> {
    if lower::find_world(source, &id.world).is_none() {
        return Err(Error::NoWorld {
            world: id.world.clone(),
        });
    }
    match lower::lower_matching(source, &id.component, |world| world == &id.world).pop() {
        Some(Ok(lowered)) => Ok(lowered),
        Some(Err(failure)) => Err(Error::Lower(lower::LowerFailures {
            failures: vec![failure],
        })),
        None => Err(Error::NotANode {
            world: id.world.clone(),
        }),
    }
}

/// Lower WIT text (one document; `name` is its pseudo-path in errors) and
/// return the contract of the world `id` names, as `id`'s component
/// provides it: the contract a catalog lowered from the same source has.
pub fn contract_from_str(
    name: &str,
    wit: &str,
    id: &ContractId,
) -> Result<ComponentContract, Error> {
    Ok(lower_world(&load::load_str(name, wit)?, id)?.contract)
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

        world provider {
            export clock;
        }
    "#;

    fn id(text: &str) -> ContractId {
        text.parse().unwrap()
    }

    #[test]
    fn a_world_is_found_by_its_ref() {
        let component: ComponentName = "pair".parse().unwrap();
        let lowered = lower::lower(&load::load_str("pair.wit", WIT).unwrap(), &component).unwrap();
        for world in ["doubler", "tripler"] {
            let expected = lowered
                .iter()
                .find(|l| l.contract.id.world.world == world)
                .map(|l| l.contract.clone())
                .unwrap();
            let found = contract_from_str(
                "pair.wit",
                WIT,
                &id(&format!("pair/demo:pair/{world}@0.1.0")),
            )
            .unwrap();
            assert_eq!(found, expected, "the catalog's contract");
        }
        let broken = format!("{WIT}\nworld broken {{ export node: func(); }}");
        let found = contract_from_str("pair.wit", &broken, &id("pair/demo:pair/broken@0.1.0"));
        assert!(
            matches!(found, Err(Error::Lower(_))),
            "a world that fails to lower reports why: {found:?}"
        );
        assert!(matches!(
            contract_from_str("pair.wit", WIT, &id("pair/demo:pair/doubler@0.2.0")),
            Err(Error::NoWorld { .. })
        ));
        assert!(matches!(
            contract_from_str("pair.wit", WIT, &id("pair/demo:pair/provider@0.1.0")),
            Err(Error::NotANode { .. })
        ));
    }

    #[test]
    fn an_encoded_package_lowers_as_its_text_does() {
        let source = load::load_str("pair.wit", WIT).unwrap();
        let encoded = load::encode(&source).unwrap();
        let decoded = load::load_encoded(&encoded).unwrap();
        let wanted = id("pair/demo:pair/doubler@0.1.0");
        assert_eq!(
            lower_world(&decoded, &wanted).unwrap().contract,
            lower_world(&source, &wanted).unwrap().contract
        );
    }
}
