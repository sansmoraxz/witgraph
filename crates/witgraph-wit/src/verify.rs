//! Checking component bytes: that untrusted bytes can be decoded at all,
//! and that a component implements a contract.

use std::collections::{BTreeMap, BTreeSet};

use witgraph_ir::interface::semver_compatible;
use witgraph_ir::{ComponentContract, PortDef};

use crate::lower::Lowered;
use crate::lower_component;

/// Why component bytes do not implement a contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The bytes are not a valid component, or their WIT could not be
    /// decoded or lowered into one node contract.
    #[error("{0}")]
    Bad(String),
    /// The bytes implement another contract: the first difference.
    #[error("{0}")]
    Mismatch(String),
}

/// Validates untrusted bytes as a component, and rejects what wit-parser's
/// decoder cannot represent: a type imported or exported (at any depth)
/// that is not a value or resource type, a function named like an
/// interface (`a:b/c`) rather than a plain or method name, a nested
/// component, a core module, or an imported instance exported again.
/// wit-parser panics on some of those, and `catch_unwind` cannot contain a
/// panic where panics abort (on `wasm32`, or in a host built with
/// `panic = "abort"`).
pub fn check_decodable(bytes: &[u8]) -> Result<(), String> {
    use wasmparser::component_types::{ComponentAnyTypeId, ComponentEntityType};
    use wasmparser::names::{ComponentName, ComponentNameKind};
    use wasmparser::{Parser, Payload, ValidPayload, Validator, WasmFeatures};

    fn check(
        name: &str,
        ty: &ComponentEntityType,
        types: wasmparser::types::TypesRef<'_>,
    ) -> Result<(), String> {
        match ty {
            // A function is a plain, constructor, method or static name.
            ComponentEntityType::Func(_) => match ComponentName::new(name, 0).map(|n| {
                matches!(
                    n.kind(),
                    ComponentNameKind::Label(_)
                        | ComponentNameKind::Constructor(_)
                        | ComponentNameKind::Method(_)
                        | ComponentNameKind::Static(_)
                )
            }) {
                Ok(true) => Ok(()),
                _ => Err(format!("function `{name}` has an interface's name")),
            },
            ComponentEntityType::Value(_) => Ok(()),
            ComponentEntityType::Type { referenced, .. } => match referenced {
                ComponentAnyTypeId::Defined(_) | ComponentAnyTypeId::Resource(_) => Ok(()),
                _ => Err("it imports or exports a component, instance or function type".into()),
            },
            ComponentEntityType::Instance(id) => types[*id]
                .exports
                .iter()
                .try_for_each(|(name, item)| check(name, &item.ty, types)),
            ComponentEntityType::Component(_) => {
                Err("it imports or exports a nested component".into())
            }
            ComponentEntityType::Module(_) => Err("it imports or exports a core module".into()),
        }
    }

    // One pass: validate every payload (function bodies are left to
    // `Component::new`, which validates them while compiling) and note the
    // root's own imports and exports, outside any nested module or
    // component.
    let mut validator = Validator::new_with_features(WasmFeatures::all());
    let mut depth = 0usize;
    let mut root_types = None;
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let invalid = |e: wasmparser::BinaryReaderError| format!("not a valid component: {e}");
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(invalid)?;
        match &payload {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader.clone() {
                    imports.push(import.map_err(invalid)?.name.name);
                }
            }
            Payload::ComponentExportSection(reader) if depth == 0 => {
                for export in reader.clone() {
                    exports.push(export.map_err(invalid)?.name.name);
                }
            }
            _ => {}
        }
        if let ValidPayload::End(types) = validator.payload(&payload).map_err(invalid)? {
            if depth == 0 {
                root_types = Some(types);
            } else {
                depth -= 1;
            }
        }
    }
    let types = root_types.ok_or("not a valid component: it has no end")?;
    let types = types.as_ref();
    let mut imported = std::collections::HashSet::new();
    for name in imports {
        if let Some(item) = types.component_item_for_import(name) {
            check(name, &item.ty, types).map_err(|e| format!("import `{name}`: {e}"))?;
            if let ComponentEntityType::Instance(id) = item.ty {
                imported.insert(id);
            }
        }
    }
    for name in exports {
        if let Some(item) = types.component_item_for_export(name) {
            check(name, &item.ty, types).map_err(|e| format!("export `{name}`: {e}"))?;
            // The decoder maps an instance's types once: an instance both
            // imported and exported would be mapped twice.
            if let ComponentEntityType::Instance(id) = item.ty
                && imported.contains(&id)
            {
                return Err(format!("export `{name}`: it exports an imported instance"));
            }
        }
    }
    Ok(())
}

/// Decodes the WIT embedded in a component, lowers it, and checks that the
/// component implements `contract`: the same `run` kind and ports, and no
/// capability import the contract does not declare. Returns the bytes' own
/// view of their world ([`lower_component`]), whose `export` is the name
/// the component exports its `node` interface under.
///
/// The capabilities are a subset check, item by item, not an equality: a
/// component built from the contract's world imports only the functions
/// and resources its code uses, and an interface pulled in only for its
/// types imports none. Every item it does import must have the signature
/// the contract declares.
pub fn verify_component(
    contract: &ComponentContract,
    bytes: &[u8],
) -> Result<Lowered, VerifyError> {
    let bad = VerifyError::Bad;
    let mismatch = VerifyError::Mismatch;
    let found = lower_component(bytes, &contract.id).map_err(|e| bad(e.to_string()))?;
    for (direction, expected, actual) in [
        ("input", &contract.inputs, &found.contract.inputs),
        ("output", &contract.outputs, &found.contract.outputs),
    ] {
        if let Some(difference) = port_difference(expected, actual) {
            return Err(mismatch(format!("{direction} {difference}")));
        }
    }
    for capability in &found.contract.capabilities {
        // wit-component merges semver-compatible imports to the newest
        // version, and wasmtime's linker resolves such an import to the
        // newest compatible definition: check against that one.
        let declared =
            witgraph_ir::interface::resolve_import(&contract.capabilities, &capability.interface);
        let Some(declared) = declared else {
            return Err(mismatch(format!(
                "the bytes import `{}`, which the contract does not declare",
                capability.interface
            )));
        };
        // A label stands for the same interface on both sides, at a
        // compatible version.
        let same_interface = match (&declared.implements, &capability.implements) {
            (None, None) => true,
            (Some(expected), Some(found)) => semver_compatible(expected, found),
            _ => false,
        };
        if !same_interface {
            let name = |i: &Option<String>| i.clone().unwrap_or_else(|| "no interface".into());
            return Err(mismatch(format!(
                "the bytes import `{}` for `{}`, the contract for `{}`",
                capability.interface,
                name(&capability.implements),
                name(&declared.implements)
            )));
        }
        for (item, signature) in &capability.items {
            match declared.items.get(item) {
                Some(expected) if expected == signature => {}
                Some(expected) => {
                    return Err(mismatch(format!(
                        "the bytes import `{}` item `{item}` as `{signature}`, the contract as `{expected}`",
                        capability.interface
                    )));
                }
                None => {
                    return Err(mismatch(format!(
                        "the bytes import `{}` item `{item}`, which the contract does not declare",
                        capability.interface
                    )));
                }
            }
        }
    }
    Ok(found)
}

/// The first difference between two sides' ports, compared by name.
fn port_difference(expected: &[PortDef], found: &[PortDef]) -> Option<String> {
    let by_name = |ports: &[PortDef]| -> BTreeMap<String, PortDef> {
        ports
            .iter()
            .map(|p| (p.name.to_string(), p.clone()))
            .collect()
    };
    let (expected, found) = (by_name(expected), by_name(found));
    let describe = |p: &PortDef| {
        let optional = if p.optional { "optional " } else { "" };
        format!("{optional}{} of {}", p.kind, p.type_display())
    };
    let names: BTreeSet<&String> = expected.keys().chain(found.keys()).collect();
    names
        .into_iter()
        .find_map(|name| match (expected.get(name), found.get(name)) {
            (Some(_), None) => Some(format!("port `{name}` is missing from the bytes")),
            (None, Some(_)) => Some(format!("port `{name}` is not in the contract")),
            (Some(e), Some(f)) if (e.kind, e.optional, &e.ty) != (f.kind, f.optional, &f.ty) => {
                Some(format!(
                    "port `{name}` is a {} in the contract but a {} in the bytes",
                    describe(e),
                    describe(f)
                ))
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_the_decoder_panics_on_are_refused() {
        let id: witgraph_ir::ComponentRef = "a:b/c@0.1.0".parse().unwrap();
        for (shape, wat) in [
            (
                "an imported instance exported again",
                r#"(component
                    (import "a:b/c" (instance $i (type $t u32) (export "t" (type (eq $t)))))
                    (export "a:b/c" (instance $i)))"#,
            ),
            (
                "a function with an interface's name",
                r#"(component (import "a:b/c" (func)))"#,
            ),
            (
                "a function with a dependency's name",
                r#"(component (import "unlocked-dep=<a:b>" (func)))"#,
            ),
        ] {
            let bytes = wat::parse_str(wat).unwrap();
            assert!(check_decodable(&bytes).is_err(), "{shape}");
            assert!(lower_component(&bytes, &id).is_err(), "{shape}");
        }
    }
}
