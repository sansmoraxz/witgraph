//! Content-hash identity for lowered component contracts.

use core::fmt::Write as _;

use sha2::{Digest, Sha256};
use witgraph_ir::{ComponentContract, PortDef, PortDirection};

/// Canonical text encoding of the structural contract.
///
/// The identity is purely structural: package, world, version, docs, and
/// declared type names are all excluded, so any two contracts with the same
/// port and capability shapes hash identically. Ports and capabilities are
/// sorted, so declaration order doesn't matter either. Drained inputs get a
/// distinct `drained-input` label, so undrained contracts keep their hashes.
/// Variable-length strings (port names, rendered types, capabilities) are
/// Debug-quoted so no crafted name can forge another line's field boundary.
///
/// The encoding leans on the `Display` renderings of
/// [`witgraph_ir::Type`] and [`witgraph_ir::PortKind`]: those spellings are
/// part of the identity, and changing them changes every hash. The leading
/// `witgraph-contract v…` line versions the encoding itself, so a deliberate
/// format change shifts hashes explicitly rather than colliding with old
/// ones.
fn canonical(contract: &ComponentContract) -> String {
    let mut out = String::from("witgraph-contract v1\n");
    write_ports(&mut out, PortDirection::Input, &contract.inputs);
    write_ports(&mut out, PortDirection::Output, &contract.outputs);
    let mut capabilities: Vec<&str> = contract
        .capabilities
        .iter()
        .map(|c| c.interface.as_str())
        .collect();
    capabilities.sort_unstable();
    for capability in capabilities {
        let _ = writeln!(out, "capability {capability:?}");
    }
    out
}

fn write_ports(out: &mut String, direction: PortDirection, ports: &[PortDef]) {
    let mut lines: Vec<String> = ports
        .iter()
        .map(|p| {
            // Draining is only meaningful on inputs; a stray flag on an
            // output must not perturb the identity.
            let label = match direction {
                PortDirection::Input if p.drained => "drained-input",
                PortDirection::Input => "input",
                PortDirection::Output => "output",
            };
            format!(
                "{label} {:?} {} {} {:?}\n",
                p.name.as_str(),
                p.kind,
                p.optional,
                p.ty.to_string()
            )
        })
        .collect();
    lines.sort();
    for line in lines {
        out.push_str(&line);
    }
}

/// Sha-256 (hex) over the canonical encoding of the lowered contract. Any
/// `content_hash` already present on the contract's id is ignored.
pub fn content_hash(contract: &ComponentContract) -> String {
    hex::encode(Sha256::digest(canonical(contract).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use witgraph_ir::{PortKind, Type};

    fn contract(port_name: &str) -> ComponentContract {
        ComponentContract {
            id: "demo:test/w@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::new(port_name, PortKind::Value, Type::F64)],
            outputs: vec![],
            capabilities: vec![],
            type_names: vec![],
            docs: None,
        }
    }

    #[test]
    fn docs_do_not_affect_identity() {
        let plain = contract("in");
        let mut documented = contract("in");
        documented.docs = Some("a doc comment".into());
        documented.inputs[0].docs = Some("port docs".into());
        assert_eq!(content_hash(&plain), content_hash(&documented));
    }

    #[test]
    fn port_rename_changes_identity() {
        assert_ne!(
            content_hash(&contract("in")),
            content_hash(&contract("inn"))
        );
    }

    #[test]
    fn drained_input_changes_identity() {
        let mut plain = contract("in");
        plain.inputs[0].kind = PortKind::Stream;
        let mut drained = plain.clone();
        drained.inputs[0].drained = true;
        assert_ne!(content_hash(&plain), content_hash(&drained));
    }

    #[test]
    fn drained_flag_on_output_does_not_affect_identity() {
        let mut plain = contract("in");
        plain.outputs = vec![PortDef::new("out", PortKind::Stream, Type::F64)];
        let mut flagged = plain.clone();
        flagged.outputs[0].drained = true;
        assert_eq!(content_hash(&plain), content_hash(&flagged));
    }

    #[test]
    fn identity_is_structural_not_nominal() {
        let a = contract("in");
        let mut b = contract("in");
        b.id = "other:pkg/x@2.0.0".parse().unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn hash_is_hex_sha256() {
        let hash = content_hash(&contract("in"));
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
