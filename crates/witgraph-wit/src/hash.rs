//! Content-hash identity for lowered component contracts.

use core::fmt::Write as _;

use sha2::{Digest, Sha256};
use wasm_wave::wasm::{WasmType, WasmTypeKind};
use witgraph_ir::{ComponentContract, PortDef, PortDirection, Type};

/// Canonical text encoding of the structural contract.
///
/// The identity is purely structural: package, world, version, docs, and
/// declared type names are all excluded, so any two contracts with the same
/// port, `run` and capability shapes hash identically. Ports and
/// capabilities are sorted, so declaration order doesn't matter either.
/// Variable-length strings (port names, rendered types, capabilities) are
/// Debug-quoted so no crafted name can forge another line's field boundary.
///
/// Payload types use this module's own canonical encoding ([`encode_type`]),
/// never wasm-wave's `Display`, so a dependency upgrade cannot shift hashes.
/// The `Display` renderings of [`witgraph_ir::PortKind`] and
/// [`witgraph_ir::RunKind`] are part of the identity too: changing those
/// spellings changes every hash. The leading
/// `witgraph-contract v…` line versions the encoding itself, so a deliberate
/// format change shifts hashes explicitly rather than colliding with old
/// ones.
fn canonical(contract: &ComponentContract) -> String {
    let mut out = String::from("witgraph-contract v2\n");
    let _ = writeln!(out, "run {}", contract.run);
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
            let payload = p.ty.as_ref().map_or_else(|| "_".into(), encode_type);
            format!(
                "{direction} {:?} {} {} {payload:?}\n",
                p.name.as_str(),
                p.kind,
                p.optional,
            )
        })
        .collect();
    lines.sort();
    for line in lines {
        out.push_str(&line);
    }
}

/// Canonical, dependency-independent rendering of a payload type. Member
/// names are Debug-quoted; an absent payload renders as `_`.
fn encode_type(ty: &Type) -> String {
    let mut out = String::new();
    write_type(&mut out, ty, &mut None);
    out
}

/// The first type kind inside `ty` that this encoding cannot represent
/// faithfully. Lowering rejects payloads for which this returns `Some`, which
/// keeps [`content_hash`] collision-free. Fixed-length lists are the case in
/// practice: wasm-wave's [`WasmType`] accessors don't expose their length.
pub(crate) fn unsupported_kind(ty: &Type) -> Option<WasmTypeKind> {
    let mut unsupported = None;
    write_type(&mut String::new(), ty, &mut unsupported);
    unsupported
}

fn write_opt(out: &mut String, ty: Option<&Type>, unsupported: &mut Option<WasmTypeKind>) {
    match ty {
        Some(ty) => write_type(out, ty, unsupported),
        None => out.push('_'),
    }
}

fn write_list<T>(
    out: &mut String,
    items: impl Iterator<Item = T>,
    mut each: impl FnMut(&mut String, T),
) {
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push(',');
        }
        each(out, item);
    }
}

fn write_type(out: &mut String, ty: &Type, unsupported: &mut Option<WasmTypeKind>) {
    let kind = ty.kind();
    let simple = match kind {
        WasmTypeKind::Bool => Some("bool"),
        WasmTypeKind::S8 => Some("s8"),
        WasmTypeKind::S16 => Some("s16"),
        WasmTypeKind::S32 => Some("s32"),
        WasmTypeKind::S64 => Some("s64"),
        WasmTypeKind::U8 => Some("u8"),
        WasmTypeKind::U16 => Some("u16"),
        WasmTypeKind::U32 => Some("u32"),
        WasmTypeKind::U64 => Some("u64"),
        WasmTypeKind::F32 => Some("f32"),
        WasmTypeKind::F64 => Some("f64"),
        WasmTypeKind::Char => Some("char"),
        WasmTypeKind::String => Some("string"),
        _ => None,
    };
    if let Some(name) = simple {
        out.push_str(name);
        return;
    }
    match kind {
        WasmTypeKind::List => {
            out.push_str("list<");
            write_opt(out, ty.list_element_type().as_ref(), unsupported);
            out.push('>');
        }
        WasmTypeKind::Record => {
            out.push_str("record{");
            write_list(out, ty.record_fields(), |out, (name, field)| {
                let _ = write!(out, "{:?}:", name.as_ref());
                write_type(out, &field, unsupported);
            });
            out.push('}');
        }
        WasmTypeKind::Tuple => {
            out.push_str("tuple<");
            write_list(out, ty.tuple_element_types(), |out, element| {
                write_type(out, &element, unsupported);
            });
            out.push('>');
        }
        WasmTypeKind::Variant => {
            out.push_str("variant{");
            write_list(out, ty.variant_cases(), |out, (name, payload)| {
                let _ = write!(out, "{:?}:", name.as_ref());
                write_opt(out, payload.as_ref(), unsupported);
            });
            out.push('}');
        }
        WasmTypeKind::Enum => {
            out.push_str("enum{");
            write_list(out, ty.enum_cases(), |out, name| {
                let _ = write!(out, "{:?}", name.as_ref());
            });
            out.push('}');
        }
        WasmTypeKind::Option => {
            out.push_str("option<");
            write_opt(out, ty.option_some_type().as_ref(), unsupported);
            out.push('>');
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            out.push_str("result<");
            write_opt(out, ok.as_ref(), unsupported);
            out.push(',');
            write_opt(out, err.as_ref(), unsupported);
            out.push('>');
        }
        WasmTypeKind::Flags => {
            out.push_str("flags{");
            write_list(out, ty.flags_names(), |out, name| {
                let _ = write!(out, "{:?}", name.as_ref());
            });
            out.push('}');
        }
        // Lowering rejects these via `unsupported_kind`; render the kind
        // by name so the encoding stays total.
        other => {
            unsupported.get_or_insert(other);
            let _ = write!(out, "?{other}");
        }
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
    use witgraph_ir::{PortKind, RunKind};

    fn contract(port_name: &str) -> ComponentContract {
        ComponentContract {
            id: "demo:test/w@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::new(port_name, PortKind::Value, Type::F64)],
            outputs: vec![],
            run: RunKind::Sync,
            capabilities: vec![],
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
    fn kind_and_run_change_identity() {
        let plain = contract("in");
        let mut streamed = contract("in");
        streamed.inputs[0].kind = PortKind::Stream;
        let mut r#async = contract("in");
        r#async.run = RunKind::Async;
        assert_ne!(content_hash(&plain), content_hash(&streamed));
        assert_ne!(content_hash(&plain), content_hash(&r#async));
    }

    #[test]
    fn direction_changes_identity() {
        let input = contract("p");
        let mut output = contract("p");
        output.outputs = std::mem::take(&mut output.inputs);
        assert_ne!(content_hash(&input), content_hash(&output));
    }

    #[test]
    fn type_encoding_is_pinned() {
        let ty = Type::record([
            ("point", Type::tuple(vec![Type::F32, Type::F32]).unwrap()),
            ("tags", Type::list(Type::STRING)),
            ("mode", Type::enum_ty(["fast", "slow"]).unwrap()),
            ("perms", Type::flags(["read"]).unwrap()),
            (
                "shape",
                Type::variant([("circle", Some(Type::F64)), ("dot", None)]).unwrap(),
            ),
            ("maybe", Type::option(Type::U8)),
            ("res", Type::result(None, Some(Type::CHAR))),
        ])
        .unwrap();
        assert_eq!(
            encode_type(&ty),
            r#"record{"point":tuple<f32,f32>,"tags":list<string>,"mode":enum{"fast","slow"},"perms":flags{"read"},"shape":variant{"circle":f64,"dot":_},"maybe":option<u8>,"res":result<_,char>}"#
        );
    }

    #[test]
    fn unit_payload_hashes_differently_from_any_type() {
        let mut unit = contract("in");
        unit.inputs = vec![PortDef::unit("in", PortKind::Stream)];
        let mut typed = contract("in");
        typed.inputs[0].kind = PortKind::Stream;
        assert_ne!(content_hash(&unit), content_hash(&typed));
    }

    #[test]
    fn canonical_encoding_is_pinned() {
        let mut c = contract("in");
        c.outputs = vec![PortDef::new("out", PortKind::Stream, Type::U32)];
        assert_eq!(
            canonical(&c),
            "witgraph-contract v2\nrun sync\ninput \"in\" value false \"f64\"\noutput \"out\" stream false \"u32\"\n"
        );
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
