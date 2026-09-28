//! Lowering resolved WIT worlds into witgraph component contracts.
//!
//! Convention: a witgraph component is a WIT world that exports an interface
//! named `node` containing up to five well-known records:
//!
//! | record           | direction | field type meaning                                   |
//! |------------------|-----------|------------------------------------------------------|
//! | `inputs`         | input     | `T` → Value; top-level `option<T>` → optional Value; |
//! |                  |           | `stream<T>` → Stream; `future<T>` → Future           |
//! | `outputs`        | output    | same mapping (no optional unwrapping)                |
//! | `input-events`   | input     | Event, field type is the payload directly            |
//! | `output-events`  | output    | Event, field type is the payload directly            |
//! | `drained-inputs` | input     | `stream<T>`/`future<T>` only: consumed to completion |
//! |                  |           | before first activation, latched as the total        |
//!
//! Top-level `option<T>` unwraps only on inputs, where it marks the port
//! optional (may be left unconnected); an output field of `option<T>` is a
//! Value whose payload is the option itself. Events have no optional form —
//! an event that never fires delivers nothing, so there is nothing for
//! `option` to add.
//!
//! The world's imported functions and function-carrying interfaces are its
//! capabilities; type-only imports are structural, not capabilities. Inputs
//! may mix sync and async kinds: any undrained async input colors the node
//! async, with Value inputs latched. A node whose async inputs are all
//! drained is sync — it fires once with the drained totals. Values and
//! Events have no completion semantics, so a `drained-inputs` field must
//! lower to Stream or Future.

use core::fmt;
use std::collections::HashSet;

use wit_parser as wp;
use wit_parser::{Resolve, TypeId, WorldItem, WorldKey};
use witgraph_ir::{
    Capability, Case, ComponentContract, ComponentRef, EnumType, Field, FlagsType, PackageRef,
    PortDef, PortDirection, PortKind, PortName, Record, Type, TypeDecl, Variant,
};

use crate::hash;
use crate::load::WitSource;

/// The well-known record names, rendered for error messages.
const WELL_KNOWN_NAMES: &str =
    "`inputs`, `outputs`, `input-events`, `output-events`, `drained-inputs`";

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
    /// The `node` interface declares a function.
    #[error(
        "the `node` interface declares a function `{function}`; a node's contract \
         is data-only — ports are record fields"
    )]
    FunctionInNodeInterface {
        /// The offending function's name.
        function: String,
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
    /// A type in the `node` interface that no port reaches.
    #[error(
        "type `{name}` in the `node` interface is neither a well-known record \
         ({WELL_KNOWN_NAMES}) nor referenced by one"
    )]
    UnreferencedType {
        /// The unreachable type's name.
        name: String,
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
    /// An event field's payload is a `stream` or `future`.
    #[error(
        "event payloads cannot be `stream`/`future` — an event delivers one \
         occurrence's payload"
    )]
    AsyncEventPayload,
    /// A `drained-inputs` field lowered to a kind with no completion.
    #[error(
        "only `stream` and `future` fields can be drained; \
         a {kind} has no completion to drain"
    )]
    NotDrainable {
        /// The kind the field lowered to.
        kind: PortKind,
    },
    /// A `stream`/`future` below the top level of a port field.
    #[error(
        "nested `stream`/`future` — async types may only appear \
         at the top level of a port field"
    )]
    NestedAsync,
    /// A resource or handle type in a payload.
    #[error("resource types are not supported in port payloads")]
    Resource,
    /// A `map` type in a payload.
    #[error("`map` types are not supported in port payloads")]
    Map,
    /// A fixed-length `list` type in a payload.
    #[error("fixed-length `list` types are not supported in port payloads")]
    FixedLengthList,
    /// An `error-context` type in a payload.
    #[error("`error-context` is not supported in port payloads")]
    ErrorContext,
    /// A type reference wit-parser could not resolve.
    #[error("unresolved type reference")]
    UnresolvedType,
    /// A payload references one of the well-known records.
    #[error("a well-known record ({WELL_KNOWN_NAMES}) cannot be used as a payload type")]
    WellKnownPayload,
    /// The same type name is bound to two structurally different types.
    #[error("type name `{name}` refers to two structurally different types")]
    ConflictingTypeName {
        /// The conflicting name.
        name: String,
    },
}

/// Lower every witgraph component world in the source's root packages.
/// Worlds that don't export a `node` interface are skipped. Every world is
/// attempted; the error reports all failing worlds, not just the first.
pub fn lower(source: &WitSource) -> Result<Vec<ComponentContract>, LowerFailures> {
    let resolve = &source.resolve;
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
            match lower_world(resolve, world, &package_ref) {
                Ok(Some(contract)) => contracts.push(contract),
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

/// How a well-known record's fields lower to ports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldRole {
    /// `T` → Value (top-level `option<T>` optional on inputs);
    /// `stream`/`future` → async kinds.
    Data,
    /// Event; the field type is the payload directly.
    Event,
    /// Like `Data`, but drained: must lower to Stream or Future.
    Drained,
}

const WELL_KNOWN: [(&str, PortDirection, FieldRole); 5] = [
    ("inputs", PortDirection::Input, FieldRole::Data),
    ("outputs", PortDirection::Output, FieldRole::Data),
    ("input-events", PortDirection::Input, FieldRole::Event),
    ("output-events", PortDirection::Output, FieldRole::Event),
    ("drained-inputs", PortDirection::Input, FieldRole::Drained),
];

fn lower_world(
    resolve: &Resolve,
    world: &wp::World,
    package: &PackageRef,
) -> Result<Option<ComponentContract>, LowerErrorKind> {
    let Some(node) = find_node_export(resolve, world)? else {
        return Ok(None);
    };
    let interface = &resolve.interfaces[node];
    if let Some(function) = interface.functions.keys().next() {
        return Err(LowerErrorKind::FunctionInNodeInterface {
            function: function.clone(),
        });
    }

    // Resolve the well-known records before lowering any field, so payload
    // lowering can reject references back into them.
    let mut consumed: HashSet<TypeId> = HashSet::new();
    let mut records = Vec::new();
    for (record_name, direction, role) in WELL_KNOWN {
        let Some(&type_id) = interface.types.get(record_name) else {
            continue;
        };
        let record = expect_record(resolve, type_id, record_name, &mut consumed)?;
        records.push((record_name, direction, role, record));
    }

    if records.is_empty() {
        return Err(LowerErrorKind::NoWellKnownRecords);
    }

    let mut lowerer = Lowerer {
        resolve,
        well_known: &consumed,
        type_names: Vec::new(),
        referenced: HashSet::new(),
    };
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for (record_name, direction, role, record) in records {
        for field in &record.fields {
            let port = lowerer.lower_port(field, direction, role, record_name)?;
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

    for (name, type_id) in &interface.types {
        if consumed.contains(type_id) || lowerer.referenced.contains(type_id) {
            continue;
        }
        return Err(LowerErrorKind::UnreferencedType { name: name.clone() });
    }

    let mut contract = ComponentContract {
        id: ComponentRef {
            package: package.clone(),
            world: world.name.clone(),
            content_hash: None,
        },
        inputs,
        outputs,
        capabilities: capabilities(resolve, world, package),
        type_names: lowerer.type_names,
        docs: world.docs.contents.clone(),
    };
    contract.id.content_hash = Some(hash::content_hash(&contract));
    Ok(Some(contract))
}

fn expect_record<'a>(
    resolve: &'a Resolve,
    mut id: TypeId,
    name: &'static str,
    consumed: &mut HashSet<TypeId>,
) -> Result<&'a wp::Record, LowerErrorKind> {
    loop {
        consumed.insert(id);
        match &resolve.types[id].kind {
            wp::TypeDefKind::Type(wp::Type::Id(next)) => id = *next,
            wp::TypeDefKind::Record(record) => return Ok(record),
            other => {
                return Err(LowerErrorKind::WellKnownNotARecord {
                    record: name,
                    found: other.as_str(),
                });
            }
        }
    }
}

/// Imported functions and function-carrying interfaces. Type-only imports
/// (bare types, function-less interfaces) demand nothing of the host, so
/// they are not capabilities.
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
                if resolve.interfaces[*id].functions.is_empty() {
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

struct Lowerer<'a> {
    resolve: &'a Resolve,
    /// Type ids of the well-known records, alias chains included.
    well_known: &'a HashSet<TypeId>,
    type_names: Vec<TypeDecl>,
    /// Ids of every type reached from a port field.
    referenced: HashSet<TypeId>,
}

impl<'a> Lowerer<'a> {
    /// Follow `type x = y` alias chains to the defining kind, if any.
    fn top_kind(&self, ty: &wp::Type) -> Option<&'a wp::TypeDefKind> {
        let mut current = *ty;
        loop {
            let wp::Type::Id(id) = current else {
                return None;
            };
            match &self.resolve.types[id].kind {
                wp::TypeDefKind::Type(inner) => current = *inner,
                kind => return Some(kind),
            }
        }
    }

    /// Record every type on the alias chain from `ty` as referenced,
    /// including chains that end in `stream`/`future`/`option` and so never
    /// reach [`Lowerer::lower_typedef`].
    fn mark_referenced(&mut self, ty: &wp::Type) {
        let mut current = *ty;
        while let wp::Type::Id(id) = current {
            self.referenced.insert(id);
            match &self.resolve.types[id].kind {
                wp::TypeDefKind::Type(inner) => current = *inner,
                _ => break,
            }
        }
    }

    fn lower_port(
        &mut self,
        field: &wp::Field,
        direction: PortDirection,
        role: FieldRole,
        record_name: &'static str,
    ) -> Result<PortDef, LowerErrorKind> {
        self.lower_field(field, direction, role)
            .map_err(|kind| LowerErrorKind::Field {
                record: record_name,
                field: field.name.clone(),
                kind,
            })
    }

    fn lower_field(
        &mut self,
        field: &wp::Field,
        direction: PortDirection,
        role: FieldRole,
    ) -> Result<PortDef, FieldErrorKind> {
        self.mark_referenced(&field.ty);
        let (kind, optional, ty) = if role == FieldRole::Event {
            if matches!(
                self.top_kind(&field.ty),
                Some(wp::TypeDefKind::Stream(_) | wp::TypeDefKind::Future(_))
            ) {
                return Err(FieldErrorKind::AsyncEventPayload);
            }
            (PortKind::Event, false, self.lower_type(&field.ty)?)
        } else {
            match self.top_kind(&field.ty) {
                Some(wp::TypeDefKind::Stream(payload)) => {
                    (PortKind::Stream, false, self.lower_payload(payload)?)
                }
                Some(wp::TypeDefKind::Future(payload)) => {
                    (PortKind::Future, false, self.lower_payload(payload)?)
                }
                Some(wp::TypeDefKind::Option(inner)) if direction == PortDirection::Input => {
                    let inner = *inner;
                    (PortKind::Value, true, self.lower_type(&inner)?)
                }
                _ => (PortKind::Value, false, self.lower_type(&field.ty)?),
            }
        };
        if role == FieldRole::Drained && !matches!(kind, PortKind::Stream | PortKind::Future) {
            return Err(FieldErrorKind::NotDrainable { kind });
        }
        Ok(PortDef {
            name: field.name.clone().into(),
            kind,
            ty,
            optional,
            drained: role == FieldRole::Drained,
            docs: field.docs.contents.clone(),
        })
    }

    fn lower_payload(&mut self, payload: &Option<wp::Type>) -> Result<Type, FieldErrorKind> {
        match payload {
            Some(ty) => {
                let ty = *ty;
                self.lower_type(&ty)
            }
            // Bare `future`/`stream`: unit payload.
            None => Ok(Type::Tuple(vec![])),
        }
    }

    fn lower_type(&mut self, ty: &wp::Type) -> Result<Type, FieldErrorKind> {
        Ok(match ty {
            wp::Type::Bool => Type::Bool,
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
            wp::Type::Char => Type::Char,
            wp::Type::String => Type::String,
            wp::Type::Id(id) => return self.lower_typedef(*id),
            wp::Type::ErrorContext => return Err(FieldErrorKind::ErrorContext),
        })
    }

    fn lower_typedef(&mut self, id: TypeId) -> Result<Type, FieldErrorKind> {
        if self.well_known.contains(&id) {
            return Err(FieldErrorKind::WellKnownPayload);
        }
        self.referenced.insert(id);
        let def = &self.resolve.types[id];
        let lowered = match &def.kind {
            wp::TypeDefKind::Type(inner) => self.lower_type(inner)?,
            wp::TypeDefKind::Record(record) => Type::Record(Record {
                fields: record
                    .fields
                    .iter()
                    .map(|f| {
                        Ok(Field {
                            name: f.name.clone(),
                            ty: self.lower_type(&f.ty)?,
                        })
                    })
                    .collect::<Result<_, FieldErrorKind>>()?,
            }),
            wp::TypeDefKind::Variant(variant) => Type::Variant(Variant {
                cases: variant
                    .cases
                    .iter()
                    .map(|c| {
                        Ok(Case {
                            name: c.name.clone(),
                            ty: c.ty.as_ref().map(|t| self.lower_type(t)).transpose()?,
                        })
                    })
                    .collect::<Result<_, FieldErrorKind>>()?,
            }),
            wp::TypeDefKind::Enum(e) => Type::Enum(EnumType {
                cases: e.cases.iter().map(|c| c.name.clone()).collect(),
            }),
            wp::TypeDefKind::Flags(f) => Type::Flags(FlagsType {
                flags: f.flags.iter().map(|f| f.name.clone()).collect(),
            }),
            wp::TypeDefKind::Option(inner) => Type::Option(Box::new(self.lower_type(inner)?)),
            wp::TypeDefKind::List(inner) => Type::List(Box::new(self.lower_type(inner)?)),
            wp::TypeDefKind::Tuple(tuple) => Type::Tuple(
                tuple
                    .types
                    .iter()
                    .map(|t| self.lower_type(t))
                    .collect::<Result<_, FieldErrorKind>>()?,
            ),
            wp::TypeDefKind::Result(result) => Type::Result {
                ok: result
                    .ok
                    .as_ref()
                    .map(|t| self.lower_type(t).map(Box::new))
                    .transpose()?,
                err: result
                    .err
                    .as_ref()
                    .map(|t| self.lower_type(t).map(Box::new))
                    .transpose()?,
            },
            wp::TypeDefKind::Future(_) | wp::TypeDefKind::Stream(_) => {
                return Err(FieldErrorKind::NestedAsync);
            }
            wp::TypeDefKind::Resource | wp::TypeDefKind::Handle(_) => {
                return Err(FieldErrorKind::Resource);
            }
            wp::TypeDefKind::Map(..) => return Err(FieldErrorKind::Map),
            wp::TypeDefKind::FixedLengthList(..) => return Err(FieldErrorKind::FixedLengthList),
            wp::TypeDefKind::Unknown => return Err(FieldErrorKind::UnresolvedType),
        };
        if let Some(name) = &def.name {
            if let Some(existing) = self.type_names.iter().find(|d| d.name == *name) {
                if existing.ty != lowered {
                    return Err(FieldErrorKind::ConflictingTypeName { name: name.clone() });
                }
            } else {
                self.type_names.push(TypeDecl {
                    name: name.clone(),
                    ty: lowered.clone(),
                    docs: def.docs.contents.clone(),
                });
            }
        }
        Ok(lowered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::load_str;
    use witgraph_ir::ConsumptionMode;

    fn lower_source(wit: &str) -> Result<Vec<ComponentContract>, crate::Error> {
        Ok(lower(&load_str("test.wit", wit)?)?)
    }

    fn lower_one(wit: &str) -> ComponentContract {
        let mut contracts = lower_source(wit).expect("lowering failed");
        assert_eq!(contracts.len(), 1, "expected exactly one component world");
        contracts.remove(0)
    }

    fn port<'a>(ports: &'a [PortDef], name: &str) -> &'a PortDef {
        ports
            .iter()
            .find(|p| p.name.as_str() == name)
            .unwrap_or_else(|| panic!("no port `{name}`"))
    }

    #[test]
    fn sync_value_ports_lower_with_full_type_coverage() {
        let contract = lower_one(
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

        assert!(contract.inputs.iter().all(|p| p.kind == PortKind::Value));
        let rate = port(&contract.inputs, "rate");
        assert!(rate.optional, "top-level option input is an optional Value");
        assert_eq!(rate.ty, Type::U32);
        assert_eq!(rate.docs.as_deref(), Some("Sampling rate."));

        let expected_point = Type::Record(Record {
            fields: vec![
                Field {
                    name: "x".into(),
                    ty: Type::F32,
                },
                Field {
                    name: "y".into(),
                    ty: Type::F32,
                },
            ],
        });
        assert_eq!(port(&contract.inputs, "pos").ty, expected_point);
        assert_eq!(
            port(&contract.inputs, "m").ty,
            Type::Enum(EnumType {
                cases: vec!["fast".into(), "slow".into()]
            })
        );
        assert_eq!(
            port(&contract.inputs, "p").ty,
            Type::Flags(FlagsType {
                flags: vec!["read".into(), "write".into()]
            })
        );
        assert_eq!(
            port(&contract.inputs, "s").ty,
            Type::Variant(Variant {
                cases: vec![
                    Case {
                        name: "circle".into(),
                        ty: Some(Type::F32)
                    },
                    Case {
                        name: "dot".into(),
                        ty: None
                    },
                ],
            })
        );
        assert_eq!(
            port(&contract.inputs, "items").ty,
            Type::List(Box::new(Type::String))
        );
        assert_eq!(
            port(&contract.inputs, "pair").ty,
            Type::Tuple(vec![Type::U8, Type::Char])
        );
        assert_eq!(
            port(&contract.inputs, "res").ty,
            Type::Result {
                ok: Some(Box::new(Type::U32)),
                err: Some(Box::new(Type::String)),
            }
        );

        let maybe = port(&contract.outputs, "maybe");
        assert!(!maybe.optional, "outputs get no optional unwrapping");
        assert_eq!(maybe.ty, Type::Option(Box::new(Type::U64)));

        let point_decl = contract
            .type_names
            .iter()
            .find(|d| d.name == "point")
            .expect("named type recorded");
        assert_eq!(point_decl.ty, expected_point);
        assert_eq!(point_decl.docs.as_deref(), Some("A 2-d point."));
    }

    #[test]
    fn async_ports_lower_to_stream_future_event() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            interface node {
                record inputs {
                    samples: stream<f64>,
                    done: future<string>,
                    tick: future,
                }
                record outputs {
                    filtered: stream<f64>,
                }
                record input-events {
                    trigger: f64,
                }
                record output-events {
                    alert: string,
                }
            }

            world async-node {
                export node;
            }
            "#,
        );

        let samples = port(&contract.inputs, "samples");
        assert_eq!((samples.kind, &samples.ty), (PortKind::Stream, &Type::F64));
        let done = port(&contract.inputs, "done");
        assert_eq!((done.kind, &done.ty), (PortKind::Future, &Type::String));
        let tick = port(&contract.inputs, "tick");
        assert_eq!(
            (tick.kind, &tick.ty),
            (PortKind::Future, &Type::Tuple(vec![])),
            "bare future carries the unit payload"
        );
        let trigger = port(&contract.inputs, "trigger");
        assert_eq!(
            (trigger.kind, &trigger.ty),
            (PortKind::Event, &Type::F64),
            "event payload is the field type directly"
        );
        assert_eq!(port(&contract.outputs, "filtered").kind, PortKind::Stream);
        assert_eq!(port(&contract.outputs, "alert").kind, PortKind::Event);
        assert!(contract.inputs.iter().all(|p| !p.drained));
        assert_eq!(contract.consumption_mode(), ConsumptionMode::Async);
    }

    #[test]
    fn aliased_stream_is_still_a_stream_port() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;

            interface node {
                type samples = stream<f64>;
                record inputs { s: samples }
            }

            world w { export node; }
            "#,
        );
        assert_eq!(port(&contract.inputs, "s").kind, PortKind::Stream);
    }

    #[test]
    fn mixed_sync_async_inputs_lower_as_async() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { v: u32, s: stream<f64> }
            }
            world mixed { export node; }
            "#,
        );
        assert_eq!(port(&contract.inputs, "v").kind, PortKind::Value);
        assert_eq!(port(&contract.inputs, "s").kind, PortKind::Stream);
        assert_eq!(
            contract.consumption_mode(),
            ConsumptionMode::Async,
            "the value input becomes a latched parameter of an async node"
        );

        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { v: u32 }
                record input-events { t: f64 }
            }
            world mixed2 { export node; }
            "#,
        );
        assert_eq!(contract.consumption_mode(), ConsumptionMode::Async);
    }

    #[test]
    fn drained_inputs_lower_drained_and_sync() {
        let drained = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record drained-inputs { s: stream<f64> }
                record inputs { threshold: u32 }
                record outputs { total: f64 }
            }
            world collector { export node; }
            "#,
        );
        let s = port(&drained.inputs, "s");
        assert!(s.drained);
        assert_eq!(s.kind, PortKind::Stream);
        assert!(!port(&drained.inputs, "threshold").drained);
        assert_eq!(
            drained.consumption_mode(),
            ConsumptionMode::Sync,
            "all async inputs drained: the node fires once with the totals"
        );

        let reactive = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { s: stream<f64>, threshold: u32 }
                record outputs { total: f64 }
            }
            world collector { export node; }
            "#,
        );
        assert_ne!(
            drained.id.content_hash, reactive.id.content_hash,
            "draining an input is part of the contract identity"
        );
    }

    #[test]
    fn drained_input_with_reactive_event_stays_async() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record drained-inputs { s: stream<f64> }
                record input-events { t: f64 }
            }
            world w { export node; }
            "#,
        );
        assert!(port(&contract.inputs, "s").drained);
        assert_eq!(
            contract.consumption_mode(),
            ConsumptionMode::Async,
            "the reactive event keeps the node async; the drain only gates first activation"
        );
    }

    #[test]
    fn value_field_in_drained_inputs_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record drained-inputs { threshold: u32 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("drained"), "{message}");
        assert!(
            message.contains("record `drained-inputs`, field `threshold`"),
            "error must locate the offending field: {message}"
        );
    }

    #[test]
    fn nested_async_rejected_with_location() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { nested: stream<stream<u8>> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("nested"), "{message}");
        assert!(
            message.contains("record `outputs`, field `nested`"),
            "error must locate the offending field: {message}"
        );
    }

    #[test]
    fn world_without_well_known_records_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record reading { value: f64 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("well-known"), "{err:#}");
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
    fn duplicate_port_name_across_records_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                record input-events { x: f64 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("duplicate input port `x`"), "{message}");
    }

    #[test]
    fn unreferenced_node_interface_type_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record input { rate: u32 }
                record outputs { out: f64 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("type `input`"), "{message}");
        assert!(
            message.contains("`inputs`"),
            "error must list the well-known names: {message}"
        );
    }

    #[test]
    fn event_stream_payload_rejected_with_location() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record input-events { t: stream<f64> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("event payloads"), "{message}");
        assert!(
            message.contains("record `input-events`, field `t`"),
            "error must locate the offending field: {message}"
        );
    }

    #[test]
    fn lowering_errors_aggregate_across_worlds() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            world a {
                export node: interface { record drained-inputs { x: u32 } }
            }
            world b {
                export node: interface {
                    record inputs { y: u32 }
                    record unused { z: u32 }
                }
            }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("world `a`"), "{message}");
        assert!(message.contains("world `b`"), "{message}");
    }

    #[test]
    fn aliased_well_known_record_lowers() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record io { x: u32 }
                type inputs = io;
            }
            world w { export node; }
            "#,
        );
        assert_eq!(port(&contract.inputs, "x").kind, PortKind::Value);
    }

    #[test]
    fn non_record_well_known_rejected_with_kind() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                enum inputs { a }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("`inputs` must be a record, found enum"),
            "{message}"
        );
    }

    #[test]
    fn bare_stream_carries_unit_payload() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { ticks: stream }
            }
            world w { export node; }
            "#,
        );
        let ticks = port(&contract.inputs, "ticks");
        assert_eq!(
            (ticks.kind, &ticks.ty),
            (PortKind::Stream, &Type::Tuple(vec![]))
        );
    }

    #[test]
    fn drained_future_lowers() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface node {
                record drained-inputs { d: future<u32> }
            }
            world w { export node; }
            "#,
        );
        let d = port(&contract.inputs, "d");
        assert!(d.drained);
        assert_eq!(d.kind, PortKind::Future);
        assert_eq!(contract.consumption_mode(), ConsumptionMode::Sync);
    }

    #[test]
    fn option_of_stream_input_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { s: option<stream<f64>> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("nested `stream`/`future`"),
            "an optional async input has no meaning — the stream itself may simply be unconnected: {message}"
        );
    }

    #[test]
    fn drained_option_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record drained-inputs { d: option<u32> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("no completion to drain"), "{message}");
    }

    #[test]
    fn map_type_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { m: map<string, u32> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("`map` types"), "{err:#}");
    }

    #[test]
    fn fixed_length_list_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { l: list<u8, 4> }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("fixed-length `list`"),
            "{err:#}"
        );
    }

    #[test]
    fn aliased_event_stream_payload_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                type s = stream<f64>;
                record input-events { t: s }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("event payloads"), "{err:#}");
    }

    #[test]
    fn duplicate_output_port_across_records_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { x: u32 }
                record output-events { x: f64 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("duplicate output port `x`"), "{message}");
    }

    #[test]
    fn well_known_record_as_payload_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                record outputs { o: inputs }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("cannot be used as a payload type"),
            "{message}"
        );
    }

    #[test]
    fn conflicting_type_names_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface d1 { record point { x: u32 } }
            interface d2 { record point { y: f64 } }
            interface node {
                use d1.{point as p1};
                use d2.{point as p2};
                record inputs { a: p1, b: p2 }
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("structurally different types"),
            "{message}"
        );
    }

    #[test]
    fn function_in_node_interface_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            interface node {
                record inputs { x: u32 }
                go: func();
            }
            world w { export node; }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("data-only"), "{message}");
        assert!(message.contains("`go`"), "{message}");
    }

    #[test]
    fn function_export_named_node_rejected() {
        let err = lower_source(
            r#"
            package demo:test@0.1.0;
            world w {
                export node: func();
            }
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
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
                }
                world inner-node { export node; }
            }

            interface node {
                record outputs { y: f64 }
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
