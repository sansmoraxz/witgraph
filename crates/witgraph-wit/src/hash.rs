//! Content-hash identity for lowered component contracts.

use core::fmt::Write as _;

use sha2::{Digest, Sha256};
use wasm_wave::wasm::{WasmType, WasmTypeKind};
use wit_parser::{Function, Handle, PackageName, Resolve, TypeDefKind, TypeId, TypeOwner};
use witgraph_ir::{Capability, ComponentContract, PortDef, PortDirection, Type};

/// Canonical text encoding of the structural contract.
///
/// The identity is purely structural: package, world, version, docs, and
/// declared type names are all excluded, so any two contracts with the same
/// port, `run` and capability shapes hash identically. A capability's shape
/// is its link name, the interface a label stands for, and every item's
/// signature. Ports, capabilities and
/// items are sorted, so declaration order doesn't matter either.
/// Variable-length strings (port names, rendered types, capabilities) are
/// Debug-quoted so no crafted name can forge another line's field boundary.
///
/// Payload types use this module's own canonical encoding ([`encode_type`]),
/// never wasm-wave's `Display`, so a dependency upgrade cannot shift hashes.
/// The `Display` rendering of [`witgraph_ir::PortKind`] is part of the
/// identity too: changing its spellings changes every hash. The leading
/// `witgraph-contract v…` line versions the encoding itself, so a deliberate
/// format change shifts hashes explicitly rather than colliding with old
/// ones.
fn canonical(contract: &ComponentContract) -> String {
    // Still v3: an `implements` line appears only for a labelled import,
    // which no v3 contract could have, so every other contract keeps its
    // hash.
    let mut out = String::from("witgraph-contract v3\n");
    // Every node's `run` is async now; the line stays so v3 hashes do not
    // shift.
    out.push_str("run async\n");
    write_ports(&mut out, PortDirection::Input, &contract.inputs);
    write_ports(&mut out, PortDirection::Output, &contract.outputs);
    let mut capabilities: Vec<&Capability> = contract.capabilities.iter().collect();
    capabilities.sort_unstable();
    for capability in capabilities {
        let _ = writeln!(out, "capability {:?}", capability.interface);
        if let Some(interface) = &capability.implements {
            let _ = writeln!(out, "implements {interface:?}");
        }
        for (name, signature) in &capability.items {
            let _ = writeln!(out, "item {name:?} {signature:?}");
        }
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
    write_type(&mut out, ty);
    out
}

fn write_opt(out: &mut String, ty: Option<&Type>) {
    match ty {
        Some(ty) => write_type(out, ty),
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

fn write_type(out: &mut String, ty: &Type) {
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
            write_opt(out, ty.list_element_type().as_ref());
            out.push('>');
        }
        WasmTypeKind::Record => {
            out.push_str("record{");
            write_list(out, ty.record_fields(), |out, (name, field)| {
                let _ = write!(out, "{:?}:", name.as_ref());
                write_type(out, &field);
            });
            out.push('}');
        }
        WasmTypeKind::Tuple => {
            out.push_str("tuple<");
            write_list(out, ty.tuple_element_types(), |out, element| {
                write_type(out, &element);
            });
            out.push('>');
        }
        WasmTypeKind::Variant => {
            out.push_str("variant{");
            write_list(out, ty.variant_cases(), |out, (name, payload)| {
                let _ = write!(out, "{:?}:", name.as_ref());
                write_opt(out, payload.as_ref());
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
            write_opt(out, ty.option_some_type().as_ref());
            out.push('>');
        }
        WasmTypeKind::Result => {
            let (ok, err) = ty.result_types().unwrap_or((None, None));
            out.push_str("result<");
            write_opt(out, ok.as_ref());
            out.push(',');
            write_opt(out, err.as_ref());
            out.push('>');
        }
        WasmTypeKind::Flags => {
            out.push_str("flags{");
            write_list(out, ty.flags_names(), |out, name| {
                let _ = write!(out, "{:?}", name.as_ref());
            });
            out.push('}');
        }
        // Lowering rejects payloads with any other kind (fixed-length
        // lists, whose length wasm-wave's accessors do not expose, say);
        // render the kind by name so the encoding stays total.
        other => {
            let _ = write!(out, "?{other}");
        }
    }
}

/// Canonical rendering of a capability function's signature: `async` when
/// it is, then every parameter (name and type) and the result. Types are
/// rendered structurally with this module's own encoding, so the same
/// signature renders identically from WIT source and from the WIT decoded
/// out of a component. A resource renders as its owning interface's id (if
/// it has one), versioned by its semver compatibility track, and its name.
/// Recursion follows the type's nesting, which lowering bounds first.
pub(crate) fn encode_function(resolve: &Resolve, function: &Function) -> String {
    let mut out = String::new();
    if function.kind.is_async() {
        out.push_str("async ");
    }
    out.push_str("func(");
    for (i, param) in function.params.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{:?}:", param.name);
        write_wit_type(&mut out, resolve, &param.ty);
    }
    out.push(')');
    if let Some(result) = &function.result {
        out.push_str("->");
        write_wit_type(&mut out, resolve, result);
    }
    out
}

fn write_wit_opt(out: &mut String, resolve: &Resolve, ty: Option<&wit_parser::Type>) {
    match ty {
        Some(ty) => write_wit_type(out, resolve, ty),
        None => out.push('_'),
    }
}

fn write_resource(out: &mut String, resolve: &Resolve, id: TypeId) {
    let def = &resolve.types[crate::lower::dealias(resolve, id)];
    // The owner's id on its semver compatibility track (`@1` for 1.x,
    // `@0.2` for 0.2.x, the exact version for 0.0.x): wit-component merges
    // compatible imports to the newest version, so a resource keeps its
    // identity across them, and only across them.
    let owner = match def.owner {
        TypeOwner::Interface(interface) => {
            let iface = &resolve.interfaces[interface];
            match (iface.package, &iface.name) {
                (Some(package), Some(name)) => {
                    let package = &resolve.packages[package].name;
                    let mut id = format!("{}:{}/{name}", package.namespace, package.name);
                    if let Some(version) = &package.version {
                        id.push('@');
                        id.push_str(&PackageName::version_compat_track_string(version));
                    }
                    Some(id)
                }
                _ => None,
            }
        }
        TypeOwner::World(_) | TypeOwner::None => None,
    };
    let _ = write!(
        out,
        "{:?}.{:?}",
        owner.unwrap_or_default(),
        def.name.as_deref().unwrap_or_default()
    );
}

fn write_wit_type(out: &mut String, resolve: &Resolve, ty: &wit_parser::Type) {
    use wit_parser::Type as T;
    let name = match ty {
        T::Bool => "bool",
        T::U8 => "u8",
        T::U16 => "u16",
        T::U32 => "u32",
        T::U64 => "u64",
        T::S8 => "s8",
        T::S16 => "s16",
        T::S32 => "s32",
        T::S64 => "s64",
        T::F32 => "f32",
        T::F64 => "f64",
        T::Char => "char",
        T::String => "string",
        T::ErrorContext => "error-context",
        T::Id(id) => {
            // Aliases followed in a loop: a chain may be far longer than
            // types nest.
            write_wit_def(out, resolve, crate::lower::dealias(resolve, *id));
            return;
        }
    };
    out.push_str(name);
}

fn write_wit_def(out: &mut String, resolve: &Resolve, id: TypeId) {
    match &resolve.types[id].kind {
        TypeDefKind::Type(ty) => write_wit_type(out, resolve, ty),
        TypeDefKind::Record(record) => {
            out.push_str("record{");
            write_list(out, record.fields.iter(), |out, field| {
                let _ = write!(out, "{:?}:", field.name);
                write_wit_type(out, resolve, &field.ty);
            });
            out.push('}');
        }
        // A bare resource in a signature is an owned handle.
        TypeDefKind::Resource => {
            out.push_str("own<");
            write_resource(out, resolve, id);
            out.push('>');
        }
        TypeDefKind::Handle(Handle::Own(resource)) => {
            out.push_str("own<");
            write_resource(out, resolve, *resource);
            out.push('>');
        }
        TypeDefKind::Handle(Handle::Borrow(resource)) => {
            out.push_str("borrow<");
            write_resource(out, resolve, *resource);
            out.push('>');
        }
        TypeDefKind::Flags(flags) => {
            out.push_str("flags{");
            write_list(out, flags.flags.iter(), |out, flag| {
                let _ = write!(out, "{:?}", flag.name);
            });
            out.push('}');
        }
        TypeDefKind::Tuple(tuple) => {
            out.push_str("tuple<");
            write_list(out, tuple.types.iter(), |out, ty| {
                write_wit_type(out, resolve, ty);
            });
            out.push('>');
        }
        TypeDefKind::Variant(variant) => {
            out.push_str("variant{");
            write_list(out, variant.cases.iter(), |out, case| {
                let _ = write!(out, "{:?}:", case.name);
                write_wit_opt(out, resolve, case.ty.as_ref());
            });
            out.push('}');
        }
        TypeDefKind::Enum(cases) => {
            out.push_str("enum{");
            write_list(out, cases.cases.iter(), |out, case| {
                let _ = write!(out, "{:?}", case.name);
            });
            out.push('}');
        }
        TypeDefKind::Option(ty) => {
            out.push_str("option<");
            write_wit_type(out, resolve, ty);
            out.push('>');
        }
        TypeDefKind::Result(result) => {
            out.push_str("result<");
            write_wit_opt(out, resolve, result.ok.as_ref());
            out.push(',');
            write_wit_opt(out, resolve, result.err.as_ref());
            out.push('>');
        }
        TypeDefKind::List(ty) => {
            out.push_str("list<");
            write_wit_type(out, resolve, ty);
            out.push('>');
        }
        TypeDefKind::FixedLengthList(ty, len) => {
            out.push_str("list<");
            write_wit_type(out, resolve, ty);
            let _ = write!(out, ",{len}>");
        }
        TypeDefKind::Map(key, value) => {
            out.push_str("map<");
            write_wit_type(out, resolve, key);
            out.push(',');
            write_wit_type(out, resolve, value);
            out.push('>');
        }
        TypeDefKind::Future(ty) => {
            out.push_str("future<");
            write_wit_opt(out, resolve, ty.as_ref());
            out.push('>');
        }
        TypeDefKind::Stream(ty) => {
            out.push_str("stream<");
            write_wit_opt(out, resolve, ty.as_ref());
            out.push('>');
        }
        TypeDefKind::Unknown => out.push('?'),
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
    use witgraph_ir::PortKind;

    fn contract(port_name: &str) -> ComponentContract {
        ComponentContract {
            id: "demo:test/w@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::new(port_name, PortKind::Value, Type::F64)],
            outputs: vec![],
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
    fn kind_changes_identity() {
        let plain = contract("in");
        let mut streamed = contract("in");
        streamed.inputs[0].kind = PortKind::Stream;
        assert_ne!(content_hash(&plain), content_hash(&streamed));
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
            "witgraph-contract v3\nrun async\ninput \"in\" value false \"f64\"\noutput \"out\" stream false \"u32\"\n"
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
