//! Checking that component bytes are what their node manifest says: that
//! they target a world of its WIT.
//!
//! Nothing about a node is read out of its bytes: its contract is its WIT
//! world's. The bytes are only checked against that world, with
//! wit-component's [`targets`], and the names they
//! import read, so a host is asked only for what they use.

use wit_component::targets;
use witgraph_ir::{Capability, WorldRef};

use crate::load::WitSource;
use crate::lower::find_world;

/// Why component bytes do not target a world.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The bytes are not a valid component.
    #[error("{0}")]
    Bad(String),
    /// The WIT has no such world.
    #[error("the WIT has no world `{0}`")]
    NoWorld(Box<WorldRef>),
    /// The bytes do not target the world: why.
    #[error("the component does not target `{world}`: {message}")]
    Mismatch {
        /// The world.
        world: Box<WorldRef>,
        /// wit-component's reason.
        message: String,
    },
}

/// The names a component imports, at its top level: interface ids (at the
/// version it imports them, which wit-component may have merged a
/// semver-compatible one into), labels, and bare functions' and resources'
/// names; with, for a label, the interface it says it stands for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imports(Vec<String>, Vec<Option<String>>);

impl Imports {
    /// The names a component imports, read off its import section.
    pub fn of(bytes: &[u8]) -> Result<Self, VerifyError> {
        let bad = |e: wasmparser::BinaryReaderError| VerifyError::Bad(format!("{e:#}"));
        let mut names = Vec::new();
        let mut implements = Vec::new();
        // Only the component's own imports: a nested component's or
        // module's are its own. Each opens with a header and closes with an
        // end of its own.
        let mut depth = 0usize;
        let mut component = false;
        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            match payload.map_err(bad)? {
                wasmparser::Payload::Version { encoding, .. } => {
                    if depth == 0 {
                        component = encoding == wasmparser::Encoding::Component;
                    }
                    depth += 1;
                }
                wasmparser::Payload::End(_) => depth = depth.saturating_sub(1),
                wasmparser::Payload::ComponentImportSection(section) if depth == 1 => {
                    for import in section {
                        let name = import.map_err(bad)?.name;
                        names.push(name.name.to_string());
                        implements.push(name.implements.map(str::to_string));
                    }
                }
                _ => {}
            }
        }
        if !component {
            return Err(VerifyError::Bad("the bytes are not a component".into()));
        }
        Ok(Self(names, implements))
    }

    /// The names, in import order.
    pub fn names(&self) -> &[String] {
        &self.0
    }

    /// The name the component imports `import` (as a contract declares it)
    /// under: itself, or the semver-compatible version wit-component merged
    /// it into. `None` when the component does not import it.
    pub fn imported_as(&self, import: &str) -> Option<&str> {
        let bare = import
            .strip_prefix("func:")
            .or_else(|| import.strip_prefix("resource:"));
        self.0
            .iter()
            .find(|name| *name == import || Some(name.as_str()) == bare)
            .or_else(|| {
                self.0
                    .iter()
                    .find(|name| witgraph_ir::interface::semver_compatible(name, import))
            })
            .map(String::as_str)
    }

    /// The capabilities of `declared` the component imports, as `declared`
    /// names them, sorted by name: what a host is asked for. One the
    /// component imports at a newer semver-compatible version is asked for
    /// as declared (the linker resolves the newer import to it).
    pub fn used(&self, declared: &[Capability]) -> Vec<Capability> {
        let mut used: Vec<Capability> = Vec::new();
        for name in &self.0 {
            let found = declared
                .iter()
                .find(|c| c.link_name() == name)
                .or_else(|| witgraph_ir::interface::resolve_import(declared, name));
            if let Some(capability) = found
                && !used.iter().any(|u| u.interface == capability.interface)
            {
                used.push(capability.clone());
            }
        }
        used.sort_by(|a, b| a.interface.cmp(&b.interface));
        used
    }
}

/// Checks that `bytes` target `world` of `source` (wit-component's
/// [`targets`], semver-compatible imports matched as wasmtime links them),
/// and returns what they import.
pub fn verify_component(
    source: &WitSource,
    world: &WorldRef,
    bytes: &[u8],
) -> Result<Imports, VerifyError> {
    let id =
        find_world(source, world).ok_or_else(|| VerifyError::NoWorld(Box::new(world.clone())))?;
    let imports = Imports::of(bytes)?;
    let mismatch = |message: String| VerifyError::Mismatch {
        world: Box::new(world.clone()),
        message,
    };
    targets(&source.resolve, id, bytes, true).map_err(|e| mismatch(format!("{e:#}")))?;
    // Types are matched structurally: a label must also stand for the
    // interface the world's does.
    let declared = crate::lower::world_capabilities(source, id);
    for (name, implements) in imports.0.iter().zip(&imports.1) {
        let Some(capability) = declared.iter().find(|c| &c.interface == name) else {
            continue;
        };
        let same = match (&capability.implements, implements) {
            (None, None) => true,
            (Some(expected), Some(found)) => {
                witgraph_ir::interface::semver_compatible(expected, found)
            }
            _ => false,
        };
        if !same {
            let what = |i: &Option<String>| i.clone().unwrap_or_else(|| "no interface".into());
            return Err(mismatch(format!(
                "the bytes import `{name}` for `{}`, the world for `{}`",
                what(implements),
                what(&capability.implements)
            )));
        }
    }
    Ok(imports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::load_str;

    const WIT: &str = r#"
        package demo:pair@0.1.0;
        interface clock { now: func() -> u64; }
        world doubler {
            import clock;
            export node: interface {
                record inputs { x: u32 }
                record outputs { y: u32 }
                run: async func(inputs: inputs) -> outputs;
            }
        }
        world tripler {
            export node: interface {
                record inputs { x: u32 }
                record outputs { y: u32, z: u32 }
                run: async func(inputs: inputs) -> outputs;
            }
        }
    "#;

    /// A component of `world`, made of a dummy module.
    fn component(source: &WitSource, world: &str) -> Vec<u8> {
        let world = find_world(source, &world.parse().unwrap()).unwrap();
        let mut module = wit_component::dummy_module(
            &source.resolve,
            world,
            wit_parser::ManglingAndAbi::Legacy(wit_parser::LiftLowerAbi::Sync),
        );
        wit_component::embed_component_metadata(
            &mut module,
            &source.resolve,
            world,
            wit_component::StringEncoding::UTF8,
            false,
        )
        .unwrap();
        wit_component::ComponentEncoder::default()
            .module(&module)
            .unwrap()
            .encode()
            .unwrap()
    }

    #[test]
    fn bytes_target_their_world_and_no_other() {
        let source = load_str("pair.wit", WIT).unwrap();
        let doubler = component(&source, "demo:pair/doubler@0.1.0");
        let imports = verify_component(
            &source,
            &"demo:pair/doubler@0.1.0".parse().unwrap(),
            &doubler,
        )
        .unwrap();
        assert_eq!(
            imports.imported_as("demo:pair/clock@0.1.0"),
            Some("demo:pair/clock@0.1.0")
        );
        assert_eq!(
            imports.imported_as("demo:pair/clock@0.1.5"),
            Some("demo:pair/clock@0.1.0")
        );
        assert!(matches!(
            verify_component(
                &source,
                &"demo:pair/tripler@0.1.0".parse().unwrap(),
                &doubler
            ),
            Err(VerifyError::Mismatch { .. })
        ));
        assert!(matches!(
            verify_component(&source, &"demo:pair/other@0.1.0".parse().unwrap(), &doubler),
            Err(VerifyError::NoWorld(_))
        ));
        assert!(matches!(
            verify_component(
                &source,
                &"demo:pair/doubler@0.1.0".parse().unwrap(),
                b"nope"
            ),
            Err(VerifyError::Bad(_))
        ));
    }
}
