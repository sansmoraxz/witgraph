//! Lowering resolved WIT worlds into witgraph component contracts.
//!
//! Convention: a witgraph component is a WIT world that exports an interface
//! named `node`. The interface declares its ports as fields of up to two
//! well-known records (at least one must be present), and exactly one
//! function, `run`:
//!
//! ```wit
//! export node: interface {
//!     record inputs  { rate: option<u32>, samples: stream<f64> }
//!     record outputs { latest: f64, total: future<f64> }
//!     run: async func(inputs: inputs) -> outputs;
//! }
//! ```
//!
//! A field's port kind comes from its type, identically on both sides:
//!
//! | field type   | port kind | notes                                          |
//! |--------------|-----------|------------------------------------------------|
//! | `T`          | Value     | read when `run` starts / latched when it returns |
//! | `option<T>`  | Value     | on `inputs` only: optional, may stay unconnected |
//! | `stream<T>`  | Stream    | bare `stream` carries the unit payload         |
//! | `future<T>`  | Future    | bare `future` carries the unit payload         |
//!
//! Top-level `option<T>` unwraps only on inputs; an output field of
//! `option<T>` is a Value whose payload is the option itself. `stream` and
//! `future` may appear only at the top level of a field.
//!
//! `run` must be `func` or `async func` (recorded as
//! [`RunKind`]). It takes exactly `(inputs: inputs)`
//! when an `inputs` record exists and no parameters otherwise, and returns
//! `outputs` when an `outputs` record exists and nothing otherwise. No other
//! function may appear in `node`.
//!
//! Payload types are resolved by wasm-wave
//! ([`wasm_wave::value::resolve_wit_type`]), so a payload is exactly what WAVE
//! can represent. Resources, handles, `map`, `error-context`, nested
//! `stream`/`future`, and fixed-length lists (whose length the contract hash
//! cannot observe) are rejected. Empty `record`/`flags`/`tuple` types parse
//! as WIT but can never appear in a component, so any in the source fail
//! every world.
//!
//! The world's imported functions and function-carrying interfaces are its
//! capabilities; type-only imports are structural, and imports from the
//! built-in `witgraph:runtime` package are provided by every witgraph host,
//! so neither counts as a capability.

use core::fmt;
use std::collections::HashSet;

use wasm_wave::value::resolve_wit_type;
use wasm_wave::wasm::{WasmTypeKind, WasmValueError};
use wit_parser as wp;
use wit_parser::{Resolve, TypeId, WorldItem, WorldKey};
use witgraph_ir::{
    Capability, ComponentContract, ComponentRef, PackageRef, PortDef, PortDirection, PortKind,
    PortName, RunKind, Type,
};

use crate::hash;
use crate::load::WitSource;

/// The well-known record names, rendered for error messages.
const WELL_KNOWN_NAMES: &str = "`inputs`, `outputs`";

/// One lowered component world: its contract plus the named WIT types its
/// ports reference. Type names are not part of the contract (payload types
/// are structural); they are kept for editor metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    /// The component's contract.
    pub contract: ComponentContract,
    /// Named types reached from the ports, in first-reference order.
    pub types: Vec<NamedType>,
}

/// A named WIT type reached from a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    /// The WIT-declared type name.
    pub name: String,
    /// The structural type the name resolves to.
    pub ty: Type,
    /// Doc comment from the WIT declaration, if any.
    pub docs: Option<String>,
}

/// One or more worlds failed to lower; every failing world is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowerFailures {
    /// The per-world failures, in package/world declaration order.
    pub failures: Vec<LowerError>,
}

impl fmt::Display for LowerFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to lower {} world(s):", self.failures.len())?;
        for failure in &self.failures {
            write!(f, "\n{failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LowerFailures {}

/// A world that could not lower to a component contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("world `{world}`: {kind}")]
pub struct LowerError {
    /// The failing world's name.
    pub world: String,
    /// Why it failed.
    pub kind: LowerErrorKind,
}

/// Why a world failed to lower; [`LowerError`] adds the world name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LowerErrorKind {
    /// The world exports `node` as a function rather than an interface.
    #[error("exports a function named `node`; the node contract must be an exported interface")]
    NodeExportedAsFunction,
    /// The world exports more than one interface named `node`.
    #[error("exports more than one interface named `node`; the node contract must be unambiguous")]
    AmbiguousNodeExport,
    /// The `node` interface declares a function other than `run`.
    #[error(
        "the `node` interface declares a function `{function}`; its only function \
         is `run` — ports are record fields"
    )]
    FunctionInNodeInterface {
        /// The offending function's name.
        function: String,
    },
    /// The `node` interface declares no `run` function.
    #[error("the `node` interface declares no `run` function")]
    MissingRun,
    /// `run` does not have the signature its records call for.
    #[error("`run` has the wrong signature: {reason}")]
    BadRunSignature {
        /// What is wrong with it.
        reason: String,
    },
    /// The `node` interface defines none of the well-known records.
    #[error(
        "exports a `node` interface but defines none of the well-known records ({WELL_KNOWN_NAMES})"
    )]
    NoWellKnownRecords,
    /// A well-known name resolves to something other than a record.
    #[error("`{record}` must be a record, found {found}")]
    WellKnownNotARecord {
        /// The well-known name.
        record: &'static str,
        /// The kind of type actually found.
        found: &'static str,
    },
    /// Two well-known records declare the same port name on the same side.
    #[error("duplicate {direction} port `{port}` — declared in more than one well-known record")]
    DuplicatePort {
        /// The side both declarations are on.
        direction: PortDirection,
        /// The duplicated port name.
        port: PortName,
    },
    /// The WIT source declares an empty `record`, `flags` or `tuple`.
    #[error("the WIT source declares an empty {what}{}; components cannot contain empty {what} types", name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default())]
    EmptyType {
        /// Which kind of type is empty.
        what: &'static str,
        /// The type's name, when it has one.
        name: Option<String>,
    },
    /// A port field failed to lower.
    #[error("record `{record}`, field `{field}`: {kind}")]
    Field {
        /// The well-known record containing the field.
        record: &'static str,
        /// The field's name.
        field: String,
        /// Why the field failed.
        kind: FieldErrorKind,
    },
}

/// Why a well-known record field failed to lower to a port;
/// [`LowerErrorKind::Field`] adds the record and field names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FieldErrorKind {
    /// A `stream`/`future` below the top level of a port field.
    #[error(
        "nested `stream`/`future` — async types may only appear \
         at the top level of a port field"
    )]
    NestedAsync,
    /// A payload type WAVE (or the contract hash) cannot represent.
    #[error("`{kind}` types are not supported in port payloads")]
    Unsupported {
        /// The offending kind, as wit-parser or wasm-wave names it.
        kind: String,
    },
}

impl From<WasmValueError> for FieldErrorKind {
    fn from(err: WasmValueError) -> Self {
        match err {
            WasmValueError::UnsupportedType(kind) if kind == "stream" || kind == "future" => {
                Self::NestedAsync
            }
            WasmValueError::UnsupportedType(kind) => Self::Unsupported { kind },
            other => Self::Unsupported {
                kind: other.to_string(),
            },
        }
    }
}

/// Lower every witgraph component world in the source's root packages.
/// Worlds that don't export a `node` interface are skipped. Every world is
/// attempted; the error reports all failing worlds, not just the first.
pub fn lower(source: &WitSource) -> Result<Vec<Lowered>, LowerFailures> {
    let resolve = &source.resolve;
    let empty = find_empty_type(resolve);
    let mut contracts = Vec::new();
    let mut failures: Vec<LowerError> = Vec::new();
    for &package_id in &source.packages {
        let package = &resolve.packages[package_id];
        let package_ref = PackageRef {
            namespace: package.name.namespace.clone(),
            name: package.name.name.clone(),
            version: package.name.version.clone(),
        };
        for &world_id in package.worlds.values() {
            let world = &resolve.worlds[world_id];
            let lowered = match &empty {
                // wasm-wave panics on these, so no world may lower past them.
                Some(kind) if find_node_export(resolve, world).is_ok_and(|n| n.is_some()) => {
                    Err(kind.clone())
                }
                _ => lower_world(resolve, world, &package_ref),
            };
            match lowered {
                Ok(Some(lowered)) => contracts.push(lowered),
                Ok(None) => {}
                Err(kind) => failures.push(LowerError {
                    world: world.name.clone(),
                    kind,
                }),
            }
        }
    }
    if failures.is_empty() {
        Ok(contracts)
    } else {
        Err(LowerFailures { failures })
    }
}

fn find_node_export(
    resolve: &Resolve,
    world: &wp::World,
) -> Result<Option<wp::InterfaceId>, LowerErrorKind> {
    let mut matches = Vec::new();
    for (key, item) in &world.exports {
        let name = match key {
            WorldKey::Name(name) => name.as_str(),
            WorldKey::Interface(iface) => match resolve.interfaces[*iface].name.as_deref() {
                Some(name) => name,
                None => continue,
            },
        };
        if name != "node" {
            continue;
        }
        match item {
            WorldItem::Interface { id, .. } => matches.push(*id),
            WorldItem::Function(_) => return Err(LowerErrorKind::NodeExportedAsFunction),
            WorldItem::Type { .. } => {}
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        _ => Err(LowerErrorKind::AmbiguousNodeExport),
    }
}

const WELL_KNOWN: [(&str, PortDirection); 2] = [
    ("inputs", PortDirection::Input),
    ("outputs", PortDirection::Output),
];

fn lower_world(
    resolve: &Resolve,
    world: &wp::World,
    package: &PackageRef,
) -> Result<Option<Lowered>, LowerErrorKind> {
    let Some(node) = find_node_export(resolve, world)? else {
        return Ok(None);
    };
    let interface = &resolve.interfaces[node];
    if let Some(function) = interface.functions.keys().find(|name| *name != "run") {
        return Err(LowerErrorKind::FunctionInNodeInterface {
            function: function.clone(),
        });
    }

    let mut consumed: HashSet<TypeId> = HashSet::new();
    let mut records = Vec::new();
    let mut record_ids: [Option<TypeId>; 2] = [None, None];
    for (slot, (record_name, direction)) in WELL_KNOWN.into_iter().enumerate() {
        let Some(&type_id) = interface.types.get(record_name) else {
            continue;
        };
        let (record, defining) = expect_record(resolve, type_id, record_name, &mut consumed)?;
        record_ids[slot] = Some(defining);
        records.push((record_name, direction, record));
    }

    if records.is_empty() {
        return Err(LowerErrorKind::NoWellKnownRecords);
    }

    let run = interface
        .functions
        .get("run")
        .ok_or(LowerErrorKind::MissingRun)?;
    let [inputs_id, outputs_id] = record_ids;
    let run = run_kind(resolve, run, inputs_id, outputs_id)?;

    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for (record_name, direction, record) in records {
        for field in &record.fields {
            let port =
                lower_field(resolve, field, direction).map_err(|kind| LowerErrorKind::Field {
                    record: record_name,
                    field: field.name.clone(),
                    kind,
                })?;
            match direction {
                PortDirection::Input => inputs.push(port),
                PortDirection::Output => outputs.push(port),
            }
        }
    }

    for (ports, direction) in [
        (&inputs, PortDirection::Input),
        (&outputs, PortDirection::Output),
    ] {
        let mut names = HashSet::new();
        for port in ports {
            if !names.insert(port.name.as_str()) {
                return Err(LowerErrorKind::DuplicatePort {
                    direction,
                    port: port.name.clone(),
                });
            }
        }
    }

    let mut contract = ComponentContract {
        id: ComponentRef {
            package: package.clone(),
            world: world.name.clone(),
            content_hash: None,
        },
        inputs,
        outputs,
        run,
        capabilities: capabilities(resolve, world, package),
        docs: world.docs.contents.clone(),
    };
    contract.id.content_hash = Some(hash::content_hash(&contract));
    Ok(Some(Lowered {
        contract,
        types: named_types(resolve, interface, &consumed),
    }))
}

/// The first empty `record`, `flags` or `tuple` anywhere in the source.
/// wit-parser accepts these, but wasm-wave cannot represent them (its
/// resolver panics) and component validation rejects them.
fn find_empty_type(resolve: &Resolve) -> Option<LowerErrorKind> {
    resolve.types.iter().find_map(|(_, def)| {
        let what = match &def.kind {
            wp::TypeDefKind::Record(r) if r.fields.is_empty() => "record",
            wp::TypeDefKind::Flags(f) if f.flags.is_empty() => "flags",
            wp::TypeDefKind::Tuple(t) if t.types.is_empty() => "tuple",
            _ => return None,
        };
        Some(LowerErrorKind::EmptyType {
            what,
            name: def.name.clone(),
        })
    })
}

/// Named value types declared in (or `use`d into) the `node` interface,
/// other than the well-known records, for editor metadata. Types WAVE cannot
/// represent are left out; ports that use them already failed to lower.
fn named_types(
    resolve: &Resolve,
    interface: &wp::Interface,
    well_known: &HashSet<TypeId>,
) -> Vec<NamedType> {
    interface
        .types
        .iter()
        .filter(|(_, id)| !well_known.contains(id))
        .filter_map(|(name, &id)| {
            let ty = resolve_wit_type(resolve, id).ok()?;
            let defining = defining_id(resolve, &wp::Type::Id(id)).unwrap_or(id);
            Some(NamedType {
                name: name.clone(),
                ty,
                docs: resolve.types[defining].docs.contents.clone(),
            })
        })
        .collect()
}

/// Follow `type x = y` alias chains to the defining kind, if any.
fn top_kind<'a>(resolve: &'a Resolve, ty: &wp::Type) -> Option<&'a wp::TypeDefKind> {
    let id = defining_id(resolve, ty)?;
    Some(&resolve.types[id].kind)
}

fn lower_field(
    resolve: &Resolve,
    field: &wp::Field,
    direction: PortDirection,
) -> Result<PortDef, FieldErrorKind> {
    let (kind, optional, ty) = match top_kind(resolve, &field.ty) {
        Some(wp::TypeDefKind::Stream(payload)) => (
            PortKind::Stream,
            false,
            payload
                .as_ref()
                .map(|t| payload_type(resolve, t))
                .transpose()?,
        ),
        Some(wp::TypeDefKind::Future(payload)) => (
            PortKind::Future,
            false,
            payload
                .as_ref()
                .map(|t| payload_type(resolve, t))
                .transpose()?,
        ),
        Some(wp::TypeDefKind::Option(inner)) if direction == PortDirection::Input => {
            (PortKind::Value, true, Some(payload_type(resolve, inner)?))
        }
        _ => (
            PortKind::Value,
            false,
            Some(payload_type(resolve, &field.ty)?),
        ),
    };
    Ok(PortDef {
        name: field.name.clone().into(),
        kind,
        ty,
        optional,
        docs: field.docs.contents.clone(),
    })
}

/// A payload type, resolved by wasm-wave. wasm-wave resolves type ids only,
/// so the primitive leaves are mapped here.
fn payload_type(resolve: &Resolve, ty: &wp::Type) -> Result<Type, FieldErrorKind> {
    let ty = match ty {
        wp::Type::Id(id) => resolve_wit_type(resolve, *id)?,
        wp::Type::Bool => Type::BOOL,
        wp::Type::U8 => Type::U8,
        wp::Type::U16 => Type::U16,
        wp::Type::U32 => Type::U32,
        wp::Type::U64 => Type::U64,
        wp::Type::S8 => Type::S8,
        wp::Type::S16 => Type::S16,
        wp::Type::S32 => Type::S32,
        wp::Type::S64 => Type::S64,
        wp::Type::F32 => Type::F32,
        wp::Type::F64 => Type::F64,
        wp::Type::Char => Type::CHAR,
        wp::Type::String => Type::STRING,
        wp::Type::ErrorContext => {
            return Err(FieldErrorKind::Unsupported {
                kind: "error-context".into(),
            });
        }
    };
    match hash::unsupported_kind(&ty) {
        Some(kind) => Err(FieldErrorKind::Unsupported {
            kind: match kind {
                WasmTypeKind::FixedLengthList => "fixed-length list".into(),
                other => other.to_string(),
            },
        }),
        None => Ok(ty),
    }
}

/// The record a well-known name resolves to, plus the id of its defining
/// type (the end of any alias chain).
fn expect_record<'a>(
    resolve: &'a Resolve,
    mut id: TypeId,
    name: &'static str,
    consumed: &mut HashSet<TypeId>,
) -> Result<(&'a wp::Record, TypeId), LowerErrorKind> {
    loop {
        consumed.insert(id);
        match &resolve.types[id].kind {
            wp::TypeDefKind::Type(wp::Type::Id(next)) => id = *next,
            wp::TypeDefKind::Record(record) => return Ok((record, id)),
            other => {
                return Err(LowerErrorKind::WellKnownNotARecord {
                    record: name,
                    found: other.as_str(),
                });
            }
        }
    }
}

/// The defining type id of `ty`, following `type x = y` aliases.
fn defining_id(resolve: &Resolve, ty: &wp::Type) -> Option<TypeId> {
    let wp::Type::Id(mut id) = *ty else {
        return None;
    };
    while let wp::TypeDefKind::Type(wp::Type::Id(next)) = resolve.types[id].kind {
        id = next;
    }
    Some(id)
}

/// Validates `run` against the records the node declares.
fn run_kind(
    resolve: &Resolve,
    run: &wp::Function,
    inputs: Option<TypeId>,
    outputs: Option<TypeId>,
) -> Result<RunKind, LowerErrorKind> {
    let bad = |reason: &str| LowerErrorKind::BadRunSignature {
        reason: reason.into(),
    };
    let kind = match run.kind {
        wp::FunctionKind::Freestanding => RunKind::Sync,
        wp::FunctionKind::AsyncFreestanding => RunKind::Async,
        _ => return Err(bad("`run` must be a plain `func` or `async func`")),
    };
    match (inputs, run.params.as_slice()) {
        (Some(id), [param])
            if param.name == "inputs" && defining_id(resolve, &param.ty) == Some(id) => {}
        (Some(_), _) => {
            return Err(bad(
                "`run` must take exactly one parameter, `inputs: inputs`",
            ));
        }
        (None, []) => {}
        (None, _) => {
            return Err(bad(
                "`run` must take no parameters when the node has no `inputs` record",
            ));
        }
    }
    match (outputs, &run.result) {
        (Some(id), Some(ty)) if defining_id(resolve, ty) == Some(id) => {}
        (Some(_), _) => return Err(bad("`run` must return `outputs`")),
        (None, None) => {}
        (None, Some(_)) => {
            return Err(bad(
                "`run` must return nothing when the node has no `outputs` record",
            ));
        }
    }
    Ok(kind)
}

/// Imported functions and function-carrying interfaces. Type-only imports
/// (bare types, function-less interfaces) demand nothing of the host, and
/// the built-in `witgraph:runtime` package is provided by every host, so
/// neither is a capability.
///
/// A named interface's capability is its full id
/// (`namespace:name/interface@version`). An anonymous inline interface has
/// no id of its own, so its capability is scoped to the importing world:
/// `namespace:name/world.import-name@version` (`@version` omitted when the
/// package is versionless). Bare function imports are prefixed `func:`.
fn capabilities(resolve: &Resolve, world: &wp::World, package: &PackageRef) -> Vec<Capability> {
    let mut capabilities: Vec<Capability> = world
        .imports
        .iter()
        .filter_map(|(key, item)| match item {
            WorldItem::Interface { id, .. } => {
                let interface = &resolve.interfaces[*id];
                if interface.functions.is_empty() || is_witgraph_runtime(resolve, interface) {
                    return None;
                }
                let name = resolve.id_of(*id).unwrap_or_else(|| {
                    let version = package
                        .version
                        .as_ref()
                        .map(|v| format!("@{v}"))
                        .unwrap_or_default();
                    format!(
                        "{}:{}/{}.{}{version}",
                        package.namespace,
                        package.name,
                        world.name,
                        resolve.name_world_key(key),
                    )
                });
                Some(Capability::new(name))
            }
            WorldItem::Function(_) => Some(Capability::new(format!(
                "func:{}",
                resolve.name_world_key(key)
            ))),
            WorldItem::Type { .. } => None,
        })
        .collect();
    capabilities.sort();
    capabilities
}

fn is_witgraph_runtime(resolve: &Resolve, interface: &wp::Interface) -> bool {
    interface.package.is_some_and(|package| {
        let name = &resolve.packages[package].name;
        name.namespace == "witgraph" && name.name == "runtime"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::load_str;
    use witgraph_ir::NodeShape;

    fn lower_all(wit: &str) -> Result<Vec<Lowered>, crate::Error> {
        Ok(lower(&load_str("test.wit", wit)?)?)
    }

    fn lower_source(wit: &str) -> Result<Vec<ComponentContract>, crate::Error> {
        Ok(lower_all(wit)?.into_iter().map(|l| l.contract).collect())
    }

    fn lower_lowered(wit: &str) -> Lowered {
        let mut lowered = lower_all(wit).expect("lowering failed");
        assert_eq!(lowered.len(), 1, "expected exactly one component world");
        lowered.remove(0)
    }

    fn lower_one(wit: &str) -> ComponentContract {
        lower_lowered(wit).contract
    }

    fn lower_err(wit: &str) -> String {
        format!("{:#}", lower_source(wit).unwrap_err())
    }

    fn port<'a>(ports: &'a [PortDef], name: &str) -> &'a PortDef {
        ports
            .iter()
            .find(|p| p.name.as_str() == name)
            .unwrap_or_else(|| panic!("no port `{name}`"))
    }

    #[test]
    fn value_ports_lower_with_full_type_coverage() {
        let Lowered { contract, types } = lower_lowered(
            r#"
            package demo:test@0.1.0;

            interface node {
                /// A 2-d point.
                record point { x: f32, y: f32 }
                type alias-point = point;
                enum mode { fast, slow }
                flags perms { read, write }
                variant shape { circle(f32), dot }

                record inputs {
                    /// Sampling rate.
                    rate: option<u32>,
                    pos: alias-point,
                    m: mode,
                    p: perms,
                    s: shape,
                    items: list<string>,
                    pair: tuple<u8, char>,
                    res: result<u32, string>,
                }
                record outputs {
                    out: point,
                    maybe: option<u64>,
                }
                run: func(inputs: inputs) -> outputs;
            }

            /// Lowers everything.
            world value-node {
                export node;
            }
            "#,
        );

        assert_eq!(contract.id.to_string(), "demo:test/value-node@0.1.0");
        assert_eq!(contract.id.content_hash.as_ref().unwrap().len(), 64);
        assert_eq!(contract.docs.as_deref(), Some("Lowers everything."));
        assert_eq!(contract.run, RunKind::Sync);
        assert_eq!(contract.shape(), NodeShape::Reactive);

        assert!(contract.inputs.iter().all(|p| p.kind == PortKind::Value));
        let rate = port(&contract.inputs, "rate");
        assert!(rate.optional, "top-level option input is an optional Value");
        assert_eq!(rate.ty, Some(Type::U32));
        assert_eq!(rate.docs.as_deref(), Some("Sampling rate."));

        let ty = |name: &str| port(&contract.inputs, name).ty.clone().unwrap();
        let expected_point = Type::record([("x", Type::F32), ("y", Type::F32)]).unwrap();
        assert_eq!(ty("pos"), expected_point);
        assert_eq!(ty("m"), Type::enum_ty(["fast", "slow"]).unwrap());
        assert_eq!(ty("p"), Type::flags(["read", "write"]).unwrap());
        assert_eq!(
            ty("s"),
            Type::variant([("circle", Some(Type::F32)), ("dot", None)]).unwrap()
        );
        assert_eq!(ty("items"), Type::list(Type::STRING));
        assert_eq!(ty("pair"), Type::tuple(vec![Type::U8, Type::CHAR]).unwrap());
        assert_eq!(ty("res"), Type::result(Some(Type::U32), Some(Type::STRING)));

        let maybe = port(&contract.outputs, "maybe");
        assert!(!maybe.optional, "outputs get no optional unwrapping");
        assert_eq!(maybe.ty, Some(Type::option(Type::U64)));

        let point_decl = types
            .iter()
            .find(|d| d.name == "point")
            .expect("named type recorded");
        assert_eq!(point_decl.ty, expected_point);
        assert_eq!(point_decl.docs.as_deref(), Some("A 2-d point."));
    }

    #[test]
    fn async_ports_lower_to_stream_and_future_on_both_sides() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            interface node {
                record inputs {
                    samples: stream<f64>,
                    done: future<string>,
                    tick: future,
                    threshold: u32,
                }
                record outputs {
                    filtered: stream<f64>,
                    total: future<f64>,
                    latest: f64,
                }
                run: async func(inputs: inputs) -> outputs;
            }

            world async-node {
                export node;
            }
            "#,
        );

        assert_eq!(contract.run, RunKind::Async);
        assert_eq!(contract.shape(), NodeShape::Streaming);
        let samples = port(&contract.inputs, "samples");
        assert_eq!(
            (samples.kind, &samples.ty),
            (PortKind::Stream, &Some(Type::F64))
        );
        let done = port(&contract.inputs, "done");
        assert_eq!(
            (done.kind, &done.ty),
            (PortKind::Future, &Some(Type::STRING))
        );
        let tick = port(&contract.inputs, "tick");
        assert_eq!(
            (tick.kind, &tick.ty),
            (PortKind::Future, &None),
            "a bare future carries no payload"
        );
        assert_eq!(port(&contract.inputs, "threshold").kind, PortKind::Value);
        assert_eq!(port(&contract.outputs, "filtered").kind, PortKind::Stream);
        assert_eq!(port(&contract.outputs, "total").kind, PortKind::Future);
        assert_eq!(port(&contract.outputs, "latest").kind, PortKind::Value);
    }

    #[test]
    fn aliased_stream_is_still_a_stream_port() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            interface node {
                type samples = stream<f64>;
                record inputs { s: samples }
                run: async func(inputs: inputs);
            }

            world w { export node; }
            "#,
        );
        assert_eq!(port(&contract.inputs, "s").kind, PortKind::Stream);
    }

    #[test]
    fn bare_stream_carries_no_payload() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { ticks: stream }
                run: async func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        let ticks = port(&contract.inputs, "ticks");
        assert_eq!((ticks.kind, &ticks.ty), (PortKind::Stream, &None));
    }

    #[test]
    fn run_kind_follows_the_function_kind() {
        let wit = |run: &str| {
            format!(
                r#"
                package demo:test@0.1.0;
                interface node {{
                    record inputs {{ x: u32 }}
                    record outputs {{ y: u32 }}
                    run: {run};
                }}
                world w {{ export node; }}
                "#
            )
        };
        let sync = lower_one(&wit("func(inputs: inputs) -> outputs"));
        let r#async = lower_one(&wit("async func(inputs: inputs) -> outputs"));
        assert_eq!(sync.run, RunKind::Sync);
        assert_eq!(r#async.run, RunKind::Async);
        assert_ne!(
            sync.id.content_hash, r#async.id.content_hash,
            "the run kind is part of the contract"
        );
    }

    #[test]
    fn run_signature_tracks_present_records() {
        let source = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { out: stream<u32> }
                run: async func() -> outputs;
            }
            world w { export node; }
            "#,
        );
        assert!(source.inputs.is_empty());

        let sink = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { items: stream<u32> }
                run: async func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(sink.outputs.is_empty());
    }

    #[test]
    fn run_through_aliased_records_lowers() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record io { x: u32 }
                type inputs = io;
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert_eq!(port(&contract.inputs, "x").kind, PortKind::Value);
    }

    #[test]
    fn missing_run_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
            }
            world w { export node; }
            "#,
        );
        assert!(message.contains("declares no `run` function"), "{message}");
    }

    #[test]
    fn bad_run_signatures_rejected() {
        let wit = |run: &str| {
            format!(
                r#"
                package demo:test@0.1.0;
                interface node {{
                    record inputs {{ x: u32 }}
                    record outputs {{ y: u32 }}
                    run: {run};
                }}
                world w {{ export node; }}
                "#
            )
        };
        for (run, expected) in [
            (
                "func() -> outputs",
                "exactly one parameter, `inputs: inputs`",
            ),
            (
                "func(x: u32) -> outputs",
                "exactly one parameter, `inputs: inputs`",
            ),
            (
                "func(i: inputs) -> outputs",
                "exactly one parameter, `inputs: inputs`",
            ),
            (
                "func(inputs: inputs, extra: u32) -> outputs",
                "exactly one parameter, `inputs: inputs`",
            ),
            ("func(inputs: inputs)", "must return `outputs`"),
            ("func(inputs: inputs) -> u32", "must return `outputs`"),
        ] {
            let message = lower_err(&wit(run));
            assert!(
                message.contains("wrong signature") && message.contains(expected),
                "`{run}`: {message}"
            );
        }

        let no_outputs = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                run: func(inputs: inputs) -> u32;
            }
            world w { export node; }
            "#,
        );
        assert!(no_outputs.contains("must return nothing"), "{no_outputs}");

        let no_inputs = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { y: u32 }
                run: func(x: u32) -> outputs;
            }
            world w { export node; }
            "#,
        );
        assert!(no_inputs.contains("must take no parameters"), "{no_inputs}");
    }

    #[test]
    fn function_other_than_run_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                run: func(inputs: inputs);
                go: func();
            }
            world w { export node; }
            "#,
        );
        assert!(message.contains("its only function"), "{message}");
        assert!(message.contains("`go`"), "{message}");
    }

    #[test]
    fn nested_async_rejected_with_location() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { nested: stream<stream<u8>> }
                run: async func() -> outputs;
            }
            world w { export node; }
            "#,
        );
        assert!(message.contains("nested"), "{message}");
        assert!(
            message.contains("record `outputs`, field `nested`"),
            "error must locate the offending field: {message}"
        );
    }

    #[test]
    fn option_of_stream_input_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { s: option<stream<f64>> }
                run: async func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(
            message.contains("nested `stream`/`future`"),
            "an optional async input has no meaning: {message}"
        );
    }

    #[test]
    fn world_without_well_known_records_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record reading { value: f64 }
                run: func();
            }
            world w { export node; }
            "#,
        );
        assert!(message.contains("well-known"), "{message}");
    }

    #[test]
    fn world_without_node_export_is_skipped() {
        let contracts = lower_source(
            r#"
            package demo:test@0.1.0;
            interface other { ping: func(); }
            world not-a-component { export other; }
            "#,
        )
        .unwrap();
        assert!(contracts.is_empty());
    }

    #[test]
    fn only_function_carrying_imports_become_capabilities() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            interface dep { ping: func(); }
            interface types-only { record cfg { threshold: u32 } }
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }

            world w {
                import dep;
                import types-only;
                import blink: func();
                use types-only.{cfg};
                export node;
            }
            "#,
        );
        assert_eq!(
            contract.capabilities,
            vec![
                Capability::new("demo:test/dep@0.1.0"),
                Capability::new("func:blink"),
            ],
            "function-less interface and bare type imports are structural, not capabilities"
        );
    }

    #[test]
    fn witgraph_runtime_imports_are_not_capabilities() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            package witgraph:runtime@0.1.0 {
                interface host {
                    fatal: func(message: string);
                }
            }

            interface dep { ping: func(); }
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }

            world w {
                import witgraph:runtime/host@0.1.0;
                import dep;
                export node;
            }
            "#,
        );
        assert_eq!(
            contract.capabilities,
            vec![Capability::new("demo:test/dep@0.1.0")],
            "the built-in runtime package is provided by every host"
        );
    }

    #[test]
    fn duplicate_port_name_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32, x: f64 }
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(
            message.contains("duplicate") && message.contains("`x`"),
            "{message}"
        );
    }

    #[test]
    fn same_name_on_both_sides_is_legal() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                record outputs { x: u32 }
                run: func(inputs: inputs) -> outputs;
            }
            world w { export node; }
            "#,
        );
        assert_eq!(contract.inputs.len(), 1);
        assert_eq!(contract.outputs.len(), 1);
    }

    #[test]
    fn lowering_errors_aggregate_across_worlds() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            world a {
                export node: interface { record inputs { x: u32 } }
            }
            world b {
                export node: interface {
                    record inputs { y: map<u32, u32> }
                    run: func(inputs: inputs);
                }
            }
            "#,
        );
        assert!(message.contains("world `a`"), "{message}");
        assert!(message.contains("world `b`"), "{message}");
    }

    #[test]
    fn non_record_well_known_rejected_with_kind() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                enum inputs { a }
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(
            message.contains("`inputs` must be a record, found enum"),
            "{message}"
        );
    }

    #[test]
    fn map_type_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { m: map<string, u32> }
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(message.contains("`map` types"), "{message}");
    }

    #[test]
    fn empty_types_fail_lowering_instead_of_panicking() {
        for (decl, what) in [
            ("record empty {}", "record"),
            ("flags empty {}", "flags"),
            ("type empty = tuple<>;", "tuple"),
        ] {
            let message = lower_err(&format!(
                r#"
                package demo:test@0.1.0;
                interface node {{
                    {decl}
                    record inputs {{ x: u32 }}
                    run: func(inputs: inputs);
                }}
                world w {{ export node; }}
                "#
            ));
            assert!(message.contains(&format!("empty {what}")), "{message}");
        }
    }

    #[test]
    fn named_types_come_from_the_node_interface() {
        let lowered = lower_lowered(
            r#"
            package demo:test@0.1.0;
            interface node {
                /// A sample.
                record reading { value: f64 }
                record inputs { r: reading }
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        let names: Vec<&str> = lowered.types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["reading"]);
        assert_eq!(lowered.types[0].docs.as_deref(), Some("A sample."));
    }

    #[test]
    fn fixed_length_list_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { l: list<u8, 4> }
                run: func(inputs: inputs);
            }
            world w { export node; }
            "#,
        );
        assert!(
            message.contains("`fixed-length list` types are not supported"),
            "{message}"
        );
    }

    #[test]
    fn function_export_named_node_rejected() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            world w {
                export node: func();
            }
            "#,
        );
        assert!(
            message.contains("exports a function named `node`"),
            "{message}"
        );
    }

    #[test]
    fn inline_interface_capability_is_world_scoped() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
            world w {
                import config: interface { get: func() -> u32; }
                export node;
            }
            "#,
        );
        assert_eq!(
            contract.capabilities,
            vec![Capability::new("demo:test/w.config@0.1.0")],
            "an anonymous inline interface is identified through the importing world"
        );
    }

    #[test]
    fn nested_package_worlds_all_lower() {
        let contracts = lower_source(
            r#"
            package demo:outer@0.1.0;

            package demo:inner@0.1.0 {
                interface node {
                    record inputs { x: u32 }
                    run: func(inputs: inputs);
                }
                world inner-node { export node; }
            }

            interface node {
                record outputs { y: f64 }
                run: func() -> outputs;
            }
            world outer-node { export node; }
            "#,
        )
        .unwrap();
        let mut ids: Vec<String> = contracts.iter().map(|c| c.id.to_string()).collect();
        ids.sort();
        assert_eq!(
            ids,
            ["demo:inner/inner-node@0.1.0", "demo:outer/outer-node@0.1.0"]
        );
    }

    #[test]
    fn versionless_package_lowers() {
        let contract = lower_one(
            r#"
            package demo:test;
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
            world w {
                import config: interface { get: func() -> u32; }
                export node;
            }
            "#,
        );
        assert_eq!(contract.id.package.version, None);
        assert_eq!(
            contract.capabilities,
            vec![Capability::new("demo:test/w.config")],
            "no version segment when the package is versionless"
        );
    }

    #[test]
    fn hash_ignores_comments_but_not_renames() {
        let base = r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { rate: u32 }
                record outputs { out: f64 }
                run: func(inputs: inputs) -> outputs;
            }
            world w { export node; }
        "#;
        let commented = r#"
            package demo:test@0.1.0;
            interface node {
                record inputs {
                    /// The rate.
                    rate: u32,
                }
                record outputs { out: f64 }
                /// Runs once.
                run: func(inputs: inputs) -> outputs;
            }
            /// Now with docs.
            world w { export node; }
        "#;
        let renamed = base.replace("out: f64", "output: f64");

        let hash = |wit: &str| lower_one(wit).id.content_hash.unwrap();
        assert_eq!(hash(base), hash(commented));
        assert_ne!(hash(base), hash(&renamed));
    }
}
