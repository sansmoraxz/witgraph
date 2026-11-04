//! Checking that a component implements a contract.

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
        let optional = if p.is_optional() { "optional " } else { "" };
        format!("{optional}{} of {}", p.kind(), p.type_display())
    };
    let names: BTreeSet<&String> = expected.keys().chain(found.keys()).collect();
    names
        .into_iter()
        .find_map(|name| match (expected.get(name), found.get(name)) {
            (Some(_), None) => Some(format!("port `{name}` is missing from the bytes")),
            (None, Some(_)) => Some(format!("port `{name}` is not in the contract")),
            // wit-component merges semver-compatible imports, so a handle's
            // version in the bytes may be another compatible one.
            (Some(e), Some(f))
                if (e.kind(), e.is_optional(), &e.ty()) != (f.kind(), f.is_optional(), &f.ty())
                    || !match (&e.handle_type(), &f.handle_type()) {
                        (Some(e), Some(f)) => e.connects(f),
                        (None, None) => true,
                        _ => false,
                    } =>
            {
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

    fn handle_at(version: &str) -> PortDef {
        PortDef::handle(
            "t",
            witgraph_ir::HandleType {
                interface: format!("test:data/tensor@{version}"),
                resource: "tensor".into(),
            },
        )
    }

    /// wit-component merges semver-compatible imports, moving a handle's version.
    #[test]
    fn a_handle_may_be_at_another_compatible_version() {
        let (expected, merged) = ([handle_at("0.1.0")], [handle_at("0.1.3")]);
        assert_eq!(port_difference(&expected, &merged), None);
        let (expected, merged) = ([handle_at("1.0.0")], [handle_at("1.4.0")]);
        assert_eq!(port_difference(&expected, &merged), None);
    }

    #[test]
    fn a_handle_may_not_be_at_an_incompatible_version() {
        let (expected, found) = ([handle_at("0.1.0")], [handle_at("0.2.0")]);
        assert!(port_difference(&expected, &found).is_some());
        let (expected, found) = ([handle_at("1.0.0")], [handle_at("2.0.0")]);
        assert!(port_difference(&expected, &found).is_some());
    }

    #[test]
    fn shapes_wit_cannot_represent_are_refused() {
        let id: witgraph_ir::ComponentRef = "a:b/c@0.1.0".parse().unwrap();
        for (shape, wat) in [
            (
                "an imported instance exported again",
                r#"(component
                    (import "a:b/c" (instance $i (type $t u32) (export "t" (type (eq $t)))))
                    (export "a:b/c" (instance $i)))"#,
            ),
            (
                "imports sharing an instance type, with no node",
                r#"(component
                    (type $i (instance (type $t u32) (export "t" (type (eq $t)))))
                    (import "a" (implements "a:b/c") (instance (type $i)))
                    (import "b" (implements "a:b/c") (instance (type $i))))"#,
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
            assert!(lower_component(&bytes, &id).is_err(), "{shape}");
        }
    }
}
