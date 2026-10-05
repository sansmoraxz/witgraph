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
//! | `stream<T>`  | Stream    | a bare `stream` carries no payload (`ty: None`)  |
//! | `future<T>`  | Future    | a bare `future` carries no payload (`ty: None`)  |
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
//! cannot observe) are rejected.
//!
//! Some WIT parses but can never appear in a component, as wasmparser
//! validates components: empty `record`/`flags`/`tuple` types; types,
//! interfaces and functions that repeat a member name (ignoring case);
//! more than 32 flags, 10,000 record fields, variant or enum cases or tuple
//! members, or 1,000 function parameters; types nested more than
//! [`MAX_TYPE_DEPTH`] levels deep; and types or functions whose effective
//! (fully expanded) size reaches [`MAX_TYPE_SIZE`]. A world that reaches
//! one (through its imports or exports) fails to lower; types no world
//! reaches are not checked. Every check runs in time linear in the WIT, and
//! before anything expands a type, so a small WIT file whose types double
//! at each step cannot make lowering take exponential time or memory.
//!
//! The world's imports that the host must implement are its capabilities,
//! each named after what a component imports:
//! - a named interface carrying functions or declaring resources, by its
//!   full id (`namespace:name/iface@version`);
//! - an anonymous inline interface, by its import name (`config`);
//! - a bare function, as `func:<name>`;
//! - a resource declared in the world itself, as `resource:<name>`, with
//!   its constructor, methods and static functions as items.
//!
//! Type-only imports are structural, and the built-in
//! `witgraph:runtime/host@0.1.x` interface is provided by every witgraph
//! host, so neither counts; importing anything else from `witgraph:runtime`
//! is an error.
//!
//! The contract lowered from the WIT source and the one decoded from a
//! component built from it agree on ports and `run`. Their capabilities
//! (and so their content hashes) agree only when the component imports
//! everything the world declares: a component imports only the items its
//! code uses, so one that uses fewer decodes to fewer capabilities. Check a
//! component against its source contract by ports, `run` and a subset of
//! capabilities, not by hash.

use core::fmt;
use std::collections::{BTreeMap, HashMap, HashSet};

use wasm_wave::value::resolve_wit_type;
use wit_parser as wp;
use wit_parser::{Resolve, TypeId, WorldItem, WorldKey};
use witgraph_ir::{
    Capability, ComponentContract, ComponentRef, PackageRef, PortDef, PortDirection, PortKind,
    RunKind, Type,
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
    /// The name the `node` interface is exported under in a component built
    /// from the world: `node` for an inline interface, the interface's full
    /// id (`namespace:name/node@version`) for a named one.
    pub export: String,
    /// Named types reached from the ports, in first-reference order.
    pub types: Vec<NamedType>,
}

/// A named WIT type reached from a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    /// The WIT-declared type name.
    pub name: String,
    /// The named interface declaring it, by full id
    /// (`namespace:name/iface@1.0`); `None` when it is declared in an
    /// anonymous inline interface (the `node` interface of
    /// `export node: interface { .. }`, say). Two types may share a name
    /// when their owners differ.
    pub owner: Option<String>,
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
    /// The failing world: its package (version included) and name, with no
    /// content hash.
    pub world: ComponentRef,
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
    /// The world imports or exports a named interface under a label
    /// (`import primary: clock;`), which the runtime's engine cannot
    /// instantiate.
    #[error(
        "{direction} `{label}` gives interface `{interface}` a label; \
         import or export the interface by its own name"
    )]
    LabeledInterface {
        /// `import` or `export`.
        direction: &'static str,
        /// The label.
        label: String,
        /// The labelled interface's name.
        interface: String,
    },
    /// The world uses `@external-id`, which the runtime's engine cannot
    /// instantiate.
    #[error(
        "`{name}` carries `@external-id`; components with it need the `cm-implements` extension, which the runtime does not enable"
    )]
    ExternalId {
        /// The item carrying it.
        name: String,
    },
    /// The world reaches a type or function with more members than a
    /// component allows.
    #[error(
        "the world reaches {what}{}{} with {count} {member}; components allow at most {max}",
        name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default(),
        owner.as_ref().map(|o| format!(" in `{o}`")).unwrap_or_default()
    )]
    TooManyMembers {
        /// Which kind of type it is, or `function`.
        what: &'static str,
        /// The type's or function's name, when it has one.
        name: Option<String>,
        /// Where it is declared (an interface id or name, or `world <name>`).
        owner: Option<String>,
        /// What it has too many of (`flags`, `fields`, `parameters`, ...).
        member: &'static str,
        /// How many it has.
        count: usize,
        /// The most a component allows.
        max: usize,
    },
    /// The world reaches a type or function whose effective size (its
    /// fully expanded structure, every use of a type counted again) reaches
    /// [`MAX_TYPE_SIZE`], or the world as a whole does: a component embeds
    /// it as one component type.
    #[error(
        "the world reaches {what}{}{} with an effective size of {size}; components allow less than {MAX_TYPE_SIZE}",
        name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default(),
        owner.as_ref().map(|o| format!(" in `{o}`")).unwrap_or_default()
    )]
    TooLarge {
        /// `type`, `function` or `world`.
        what: &'static str,
        /// The type's, function's or world's name, when it has one.
        name: Option<String>,
        /// Where it is declared (an interface id or name, or `world <name>`).
        owner: Option<String>,
        /// Its effective size, or a lower bound past the limit.
        size: u64,
    },
    /// The world reaches a type, an interface or a function that repeats a
    /// member name (ignoring case).
    #[error(
        "the world reaches {what}{}{} with duplicate {member} `{duplicate}`; \
         components cannot contain such types",
        name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default(),
        owner.as_ref().map(|o| format!(" in `{o}`")).unwrap_or_default()
    )]
    DuplicateName {
        /// Which kind of type it is (or `interface`, or `function`).
        what: &'static str,
        /// The type's (interface's, function's) name, when it has one.
        name: Option<String>,
        /// Where it is declared (an interface id or name, or `world <name>`).
        owner: Option<String>,
        /// What kind of member repeats (`field`, `case`, `flag`,
        /// `parameter`, ...).
        member: &'static str,
        /// The repeated name.
        duplicate: String,
    },
    /// The world reaches an empty `record`, `flags` or `tuple`.
    #[error(
        "the world reaches an empty {what}{}{}; components cannot contain empty {what} types",
        name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default(),
        owner.as_ref().map(|o| format!(" in `{o}`")).unwrap_or_default()
    )]
    EmptyType {
        /// Which kind of type is empty.
        what: &'static str,
        /// The type's name, when it has one.
        name: Option<String>,
        /// Where it is declared (an interface id or name, or `world <name>`).
        owner: Option<String>,
    },
    /// The world reaches a type nested more than [`MAX_TYPE_DEPTH`] levels
    /// deep, past what a component may contain.
    #[error(
        "the world reaches type{}{} nested {depth} levels deep; components allow at most {MAX_TYPE_DEPTH}",
        name.as_ref().map(|n| format!(" `{n}`")).unwrap_or_default(),
        owner.as_ref().map(|o| format!(" in `{o}`")).unwrap_or_default()
    )]
    TooDeep {
        /// The type's name, when it has one.
        name: Option<String>,
        /// Where it is declared (an interface id or name, or `world <name>`).
        owner: Option<String>,
        /// How deep it nests.
        depth: usize,
    },
    /// The world imports something from the built-in `witgraph:runtime`
    /// package other than its `host@0.1.x` interface as every host provides
    /// it (`fatal: func(message: string)`).
    #[error(
        "import `{interface}`: {reason}; hosts provide only `witgraph:runtime/host@0.1.x` with `fatal: func(message: string)`"
    )]
    RuntimeImport {
        /// The imported interface's id.
        interface: String,
        /// What differs.
        reason: String,
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

/// The deepest a type may nest: the component model's limit on type
/// nesting (wasmparser's `MAX_WASM_COMPONENT_TYPE_DEPTH`), counted as
/// wasmparser counts it: a primitive, `flags`, `enum` or handle is 1 deep,
/// any other type one more than its deepest member, and an alias as deep
/// as what it aliases. Checking it before any recursive walk also keeps
/// lowering from overflowing the stack on adversarial WIT.
pub const MAX_TYPE_DEPTH: usize = 100;

/// The effective size a type or function must stay under: wasmparser's
/// `MAX_WASM_TYPE_SIZE`. A primitive, `flags`, `enum` or handle has size 1,
/// any other type 1 plus the sizes of its members (a type used twice counts
/// twice), an alias the size of what it aliases, and a function 1 plus the
/// sizes of its parameters and result. The whole world counts too: a
/// component embeds it as one component type, of size 1 plus its imports
/// and exports, an interface 1 plus its types and functions.
pub const MAX_TYPE_SIZE: u64 = 1_000_000;

/// The most fields a record, cases a variant or enum, or members a tuple
/// may have in a component (wasmparser's limits).
const MAX_MEMBERS: usize = 10_000;

/// The most flags a `flags` type may have.
const MAX_FLAGS: usize = 32;

/// The most parameters a function may have in a component.
const MAX_PARAMS: usize = 1_000;

/// Lower every witgraph component world in the source's root packages.
/// Worlds that don't export a `node` interface are skipped. Every world is
/// attempted; the error reports all failing worlds, not just the first, and
/// discards the worlds that lowered. [`lower_each`] keeps them.
pub fn lower(source: &WitSource) -> Result<Vec<Lowered>, LowerFailures> {
    let mut contracts = Vec::new();
    let mut failures: Vec<LowerError> = Vec::new();
    for result in lower_each(source) {
        match result {
            Ok(lowered) => contracts.push(lowered),
            Err(failure) => failures.push(failure),
        }
    }
    if failures.is_empty() {
        Ok(contracts)
    } else {
        Err(LowerFailures { failures })
    }
}

/// Like [`lower`], but one result per component world, in package/world
/// declaration order: a world that fails does not discard the others.
pub fn lower_each(source: &WitSource) -> Vec<Result<Lowered, LowerError>> {
    let resolve = &source.resolve;
    let mut results = Vec::new();
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
                Ok(Some(lowered)) => results.push(Ok(lowered)),
                Ok(None) => {}
                Err(kind) => results.push(Err(LowerError {
                    world: ComponentRef {
                        package: package_ref.clone(),
                        world: world.name.clone(),
                        content_hash: None,
                    },
                    kind,
                })),
            }
        }
    }
    results
}

/// The world's `node` export: its interface and the name it is exported
/// under in a component.
fn find_node_export<'a>(
    resolve: &Resolve,
    world: &'a wp::World,
) -> Result<Option<(wp::InterfaceId, &'a WorldKey)>, LowerErrorKind> {
    let mut matches = Vec::new();
    for (key, item) in &world.exports {
        let name = match key {
            WorldKey::Name(name) => name.as_str(),
            WorldKey::Interface(iface) => match resolve.interfaces[*iface].name.as_deref() {
                Some(name) => name,
                None => continue,
            },
        };
        // `export primary: node;` is a `node` too, under a label that
        // `check_labels` then rejects.
        let labelled_node = matches!(
            item,
            WorldItem::Interface { id, .. } if resolve.interfaces[*id].name.as_deref() == Some("node")
        );
        if name != "node" && !labelled_node {
            continue;
        }
        match item {
            WorldItem::Interface { id, .. } => matches.push((*id, key)),
            WorldItem::Function(_) => return Err(LowerErrorKind::NodeExportedAsFunction),
            WorldItem::Type { .. } => {}
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => Err(LowerErrorKind::AmbiguousNodeExport),
    }
}

/// Rejects names that differ only in letter case within an interface the
/// world imports or exports (its functions and types). wit-parser accepts
/// them there (it rejects them among a world's own imports and exports),
/// but component validation compares names ignoring case.
fn check_case_clashes(resolve: &Resolve, world: &wp::World) -> Result<(), LowerErrorKind> {
    for item in world.imports.values().chain(world.exports.values()) {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let interface = &resolve.interfaces[*id];
        let names = interface
            .functions
            .keys()
            .chain(interface.types.keys())
            .map(String::as_str);
        if let Some(name) = first_repeat(names) {
            return Err(LowerErrorKind::DuplicateName {
                what: "interface",
                name: interface.name.clone(),
                owner: interface_id(resolve, *id),
                member: "function or type",
                duplicate: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Rejects `@external-id` on anything a component built from the world
/// would encode: the world's imports and exports, and the functions and
/// types of the interfaces it imports and exports. Components encode it
/// with the `cm-implements` extension, which the runtime does not enable.
fn check_external_ids(resolve: &Resolve, world: &wp::World) -> Result<(), LowerErrorKind> {
    let found = |name: String| Err(LowerErrorKind::ExternalId { name });
    for (key, item) in world.imports.iter().chain(&world.exports) {
        if resolve.external_id_value(key, item).is_some() {
            return found(resolve.name_world_key(key));
        }
        if let WorldItem::Interface { id, .. } = item {
            let interface = &resolve.interfaces[*id];
            if let Some((name, _)) = interface
                .functions
                .iter()
                .find(|(_, f)| f.external_id.is_some())
            {
                return found(name.clone());
            }
            if let Some((name, _)) = interface
                .types
                .iter()
                .find(|(_, ty)| resolve.types[**ty].external_id.is_some())
            {
                return found(name.clone());
            }
        }
    }
    Ok(())
}

/// Rejects a named interface imported or exported under a label
/// (`import primary: clock;`). Components encode those with the
/// `cm-implements` extension, which the runtime does not enable.
fn check_labels(resolve: &Resolve, world: &wp::World) -> Result<(), LowerErrorKind> {
    let items = world
        .imports
        .iter()
        .map(|item| ("import", item))
        .chain(world.exports.iter().map(|item| ("export", item)));
    for (direction, (key, item)) in items {
        if let (WorldKey::Name(label), WorldItem::Interface { id, .. }) = (key, item)
            && let Some(interface) = &resolve.interfaces[*id].name
        {
            return Err(LowerErrorKind::LabeledInterface {
                direction,
                label: label.clone(),
                interface: interface.clone(),
            });
        }
    }
    Ok(())
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
    let Some((node, export)) = find_node_export(resolve, world)? else {
        return Ok(None);
    };
    check_labels(resolve, world)?;
    check_external_ids(resolve, world)?;
    check_case_clashes(resolve, world)?;
    check_runtime_imports(resolve, world)?;
    // Before anything walks a type recursively (wasm-wave's resolver, the
    // hash), so a malformed or adversarial type is an error, not a panic or
    // a stack overflow.
    check_world_types(resolve, world)?;
    let interface = &resolve.interfaces[node];
    if let Some(function) = interface.functions.keys().find(|name| *name != "run") {
        return Err(LowerErrorKind::FunctionInNodeInterface {
            function: function.clone(),
        });
    }

    let mut records = Vec::new();
    let mut record_ids: [Option<TypeId>; 2] = [None, None];
    for (slot, (record_name, direction)) in WELL_KNOWN.into_iter().enumerate() {
        let Some(&type_id) = interface.types.get(record_name) else {
            continue;
        };
        let (record, defining) = expect_record(resolve, type_id, record_name)?;
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
    let mut port_types = Vec::new();
    for (record_name, direction, record) in records {
        for field in &record.fields {
            port_types.push(&field.ty);
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

    let mut contract = ComponentContract {
        id: ComponentRef {
            package: package.clone(),
            world: world.name.clone(),
            content_hash: None,
        },
        inputs,
        outputs,
        run,
        capabilities: capabilities(resolve, world),
        docs: world.docs.contents.clone(),
    };
    contract.id.content_hash = Some(hash::content_hash(&contract));
    Ok(Some(Lowered {
        contract,
        export: resolve.name_world_key(export),
        types: named_types(
            resolve,
            &port_types,
            &WELL_KNOWN
                .iter()
                .filter_map(|(name, _)| interface.types.get(*name).copied())
                .collect(),
        ),
    }))
}

/// The first name repeated, ignoring ASCII case (as component validation
/// compares names).
fn first_repeat<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .find(|name| !seen.insert(name.to_ascii_lowercase()))
}

/// The defining type of `id`: the end of its `type x = y` alias chain.
pub(crate) fn dealias(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let wp::TypeDefKind::Type(wp::Type::Id(next)) = resolve.types[id].kind {
        id = next;
    }
    id
}

/// The id of a named interface, if it has one.
fn interface_id(resolve: &Resolve, interface: wp::InterfaceId) -> Option<String> {
    resolve.id_of(interface)
}

/// Where a type is declared, for error messages: an interface's id (or
/// name), or `world <name>`.
fn type_owner(resolve: &Resolve, def: &wp::TypeDef) -> Option<String> {
    match def.owner {
        wp::TypeOwner::Interface(interface) => {
            interface_id(resolve, interface).or_else(|| resolve.interfaces[interface].name.clone())
        }
        wp::TypeOwner::World(world) => Some(format!("world {}", resolve.worlds[world].name)),
        wp::TypeOwner::None => None,
    }
}

/// The type a [`wp::Type`] refers to, unless it is a primitive.
fn type_id(ty: &wp::Type) -> Option<TypeId> {
    match ty {
        wp::Type::Id(id) => Some(*id),
        _ => None,
    }
}

/// The member types a type definition refers to directly, as written: a
/// field's type once per field. Every enumeration of a type's members goes
/// through here, and the match is exhaustive, so a new kind of type is a
/// compile error, not a silently skipped member. A handle refers to its
/// resource by id rather than as a member ([`handle_resource`]).
fn child_types(def: &wp::TypeDef) -> Vec<&wp::Type> {
    match &def.kind {
        wp::TypeDefKind::Record(record) => record.fields.iter().map(|f| &f.ty).collect(),
        wp::TypeDefKind::Tuple(tuple) => tuple.types.iter().collect(),
        wp::TypeDefKind::Variant(variant) => {
            variant.cases.iter().filter_map(|c| c.ty.as_ref()).collect()
        }
        wp::TypeDefKind::Option(ty)
        | wp::TypeDefKind::List(ty)
        | wp::TypeDefKind::FixedLengthList(ty, _)
        | wp::TypeDefKind::Type(ty) => vec![ty],
        wp::TypeDefKind::Result(result) => result.ok.iter().chain(&result.err).collect(),
        wp::TypeDefKind::Map(key, value) => vec![key, value],
        wp::TypeDefKind::Future(ty) | wp::TypeDefKind::Stream(ty) => ty.iter().collect(),
        wp::TypeDefKind::Resource
        | wp::TypeDefKind::Handle(_)
        | wp::TypeDefKind::Flags(_)
        | wp::TypeDefKind::Enum(_)
        | wp::TypeDefKind::Unknown => Vec::new(),
    }
}

/// The resource a handle type refers to.
fn handle_resource(def: &wp::TypeDef) -> Option<TypeId> {
    match def.kind {
        wp::TypeDefKind::Handle(wp::Handle::Own(resource) | wp::Handle::Borrow(resource)) => {
            Some(resource)
        }
        _ => None,
    }
}

/// The type ids a type definition refers to directly: its member types'
/// and, for a handle, its resource.
fn children(def: &wp::TypeDef) -> impl Iterator<Item = TypeId> + '_ {
    child_types(def)
        .into_iter()
        .filter_map(type_id)
        .chain(handle_resource(def))
}

/// Every function a world's imports and exports reach, with where it is
/// declared (an interface id or name, or `world <name>`).
fn world_functions<'a>(
    resolve: &'a Resolve,
    world: &'a wp::World,
) -> Vec<(Option<String>, &'a wp::Function)> {
    let mut out = Vec::new();
    for item in world.imports.values().chain(world.exports.values()) {
        match item {
            WorldItem::Interface { id, .. } => {
                let interface = &resolve.interfaces[*id];
                let owner = interface_id(resolve, *id).or_else(|| interface.name.clone());
                out.extend(interface.functions.values().map(|f| (owner.clone(), f)));
            }
            WorldItem::Function(f) => out.push((Some(format!("world {}", world.name)), f)),
            WorldItem::Type { .. } => {}
        }
    }
    out
}

/// Every type a world's imports and exports reach, each once (found
/// without recursion).
fn world_types(resolve: &Resolve, world: &wp::World) -> Vec<TypeId> {
    let mut stack: Vec<TypeId> = Vec::new();
    for item in world.imports.values().chain(world.exports.values()) {
        match item {
            WorldItem::Interface { id, .. } => {
                stack.extend(resolve.interfaces[*id].types.values().copied());
            }
            WorldItem::Type { id, .. } => stack.push(*id),
            WorldItem::Function(_) => {}
        }
    }
    for (_, f) in world_functions(resolve, world) {
        stack.extend(f.params.iter().filter_map(|p| type_id(&p.ty)));
        stack.extend(f.result.as_ref().and_then(type_id));
    }
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            order.push(id);
            stack.extend(children(&resolve.types[id]));
        }
    }
    order
}

/// How deep a type nests and how large it is, as wasmparser counts them
/// (see [`MAX_TYPE_DEPTH`] and [`MAX_TYPE_SIZE`]).
#[derive(Debug, Clone, Copy)]
struct Measure {
    depth: usize,
    size: u64,
}

/// A primitive, `flags`, `enum`, handle, bare `stream` or `future`.
const LEAF: Measure = Measure { depth: 1, size: 1 };

/// The measure of every type in `ids`, and of everything they reach, each
/// computed once from its members' (so in time linear in the types, however
/// often they reuse each other), without recursion. Sizes saturate.
fn measures(resolve: &Resolve, ids: &[TypeId]) -> HashMap<TypeId, Measure> {
    let mut measured: HashMap<TypeId, Measure> = HashMap::new();
    for &root in ids {
        // (id, members pushed)
        let mut stack = vec![(root, false)];
        while let Some((id, expanded)) = stack.pop() {
            if measured.contains_key(&id) {
                continue;
            }
            let def = &resolve.types[id];
            if expanded {
                let of = |ty: &wp::Type| match type_id(ty) {
                    None => LEAF,
                    Some(id) => measured.get(&id).copied().unwrap_or(LEAF),
                };
                let measure = match &def.kind {
                    // An alias is what it aliases.
                    wp::TypeDefKind::Type(ty) => of(ty),
                    _ => child_types(def)
                        .into_iter()
                        .map(of)
                        .fold(LEAF, |acc, m| Measure {
                            depth: acc.depth.max(m.depth + 1),
                            size: acc.size.saturating_add(m.size),
                        }),
                };
                measured.insert(id, measure);
            } else {
                stack.push((id, true));
                // WIT types are acyclic (a handle ends at its resource,
                // which has no members), so this terminates.
                stack.extend(
                    child_types(def)
                        .into_iter()
                        .filter_map(type_id)
                        .filter(|k| !measured.contains_key(k))
                        .map(|k| (k, false)),
                );
            }
        }
    }
    measured
}

/// Checks every type and function the world reaches against what a
/// component may contain (see the module docs): no empty
/// `record`/`flags`/`tuple` (wasm-wave panics on them), no repeated member
/// names, member counts and nesting within wasmparser's limits, and
/// effective sizes under [`MAX_TYPE_SIZE`].
fn check_world_types(resolve: &Resolve, world: &wp::World) -> Result<(), LowerErrorKind> {
    let reached = world_types(resolve, world);
    let measured = measures(resolve, &reached);
    for &id in &reached {
        let def = &resolve.types[id];
        let owner = || type_owner(resolve, def);
        let empty = match &def.kind {
            wp::TypeDefKind::Record(r) if r.fields.is_empty() => Some("record"),
            wp::TypeDefKind::Flags(f) if f.flags.is_empty() => Some("flags"),
            wp::TypeDefKind::Tuple(t) if t.types.is_empty() => Some("tuple"),
            _ => None,
        };
        if let Some(what) = empty {
            return Err(LowerErrorKind::EmptyType {
                what,
                name: def.name.clone(),
                owner: owner(),
            });
        }
        let count = match &def.kind {
            wp::TypeDefKind::Record(r) => Some(("record", "fields", r.fields.len(), MAX_MEMBERS)),
            wp::TypeDefKind::Variant(v) => Some(("variant", "cases", v.cases.len(), MAX_MEMBERS)),
            wp::TypeDefKind::Enum(e) => Some(("enum", "cases", e.cases.len(), MAX_MEMBERS)),
            wp::TypeDefKind::Tuple(t) => Some(("tuple", "members", t.types.len(), MAX_MEMBERS)),
            wp::TypeDefKind::Flags(f) => Some(("flags", "flags", f.flags.len(), MAX_FLAGS)),
            _ => None,
        };
        if let Some((what, member, count, max)) = count
            && count > max
        {
            return Err(LowerErrorKind::TooManyMembers {
                what,
                name: def.name.clone(),
                owner: owner(),
                member,
                count,
                max,
            });
        }
        let repeat = match &def.kind {
            wp::TypeDefKind::Record(r) => first_repeat(r.fields.iter().map(|f| f.name.as_str()))
                .map(|d| ("record", "field", d)),
            wp::TypeDefKind::Variant(v) => first_repeat(v.cases.iter().map(|c| c.name.as_str()))
                .map(|d| ("variant", "case", d)),
            wp::TypeDefKind::Enum(e) => {
                first_repeat(e.cases.iter().map(|c| c.name.as_str())).map(|d| ("enum", "case", d))
            }
            wp::TypeDefKind::Flags(f) => {
                first_repeat(f.flags.iter().map(|f| f.name.as_str())).map(|d| ("flags", "flag", d))
            }
            _ => None,
        };
        if let Some((what, member, duplicate)) = repeat {
            return Err(LowerErrorKind::DuplicateName {
                what,
                name: def.name.clone(),
                owner: owner(),
                member,
                duplicate: duplicate.to_string(),
            });
        }
        if let Some(measure) = measured.get(&id) {
            if measure.depth > MAX_TYPE_DEPTH {
                return Err(LowerErrorKind::TooDeep {
                    name: def.name.clone(),
                    owner: owner(),
                    depth: measure.depth,
                });
            }
            if measure.size >= MAX_TYPE_SIZE {
                return Err(LowerErrorKind::TooLarge {
                    what: "type",
                    name: def.name.clone(),
                    owner: owner(),
                    size: measure.size,
                });
            }
        }
    }
    for (owner, function) in world_functions(resolve, world) {
        let name = || Some(function.name.clone());
        if function.params.len() > MAX_PARAMS {
            return Err(LowerErrorKind::TooManyMembers {
                what: "function",
                name: name(),
                owner,
                member: "parameters",
                count: function.params.len(),
                max: MAX_PARAMS,
            });
        }
        if let Some(duplicate) = first_repeat(function.params.iter().map(|p| p.name.as_str())) {
            return Err(LowerErrorKind::DuplicateName {
                what: "function",
                name: name(),
                owner,
                member: "parameter",
                duplicate: duplicate.to_string(),
            });
        }
        let size = function_size(&measured, function);
        if size >= MAX_TYPE_SIZE {
            return Err(LowerErrorKind::TooLarge {
                what: "function",
                name: name(),
                owner,
                size,
            });
        }
    }
    // A component built from the world embeds the whole world as one
    // component type, which must stay under the limit too.
    let size = world_size(resolve, world, &measured);
    if size >= MAX_TYPE_SIZE {
        return Err(LowerErrorKind::TooLarge {
            what: "world",
            name: Some(world.name.clone()),
            owner: None,
            size,
        });
    }
    Ok(())
}

/// The size of `ty`, from the measures of the types it may refer to.
fn type_size(measured: &HashMap<TypeId, Measure>, ty: &wp::Type) -> u64 {
    type_id(ty)
        .and_then(|id| measured.get(&id))
        .map_or(LEAF.size, |m| m.size)
}

/// A function's effective size: 1 plus its parameters' and result's.
fn function_size(measured: &HashMap<TypeId, Measure>, function: &wp::Function) -> u64 {
    function
        .params
        .iter()
        .map(|p| type_size(measured, &p.ty))
        .chain(function.result.as_ref().map(|ty| type_size(measured, ty)))
        .fold(1, u64::saturating_add)
}

/// The effective size of the component type that encodes the whole world,
/// as wit-component embeds it in every component built from the world: 1,
/// plus each import and export, an interface counting 1 plus each of its
/// types and functions.
fn world_size(resolve: &Resolve, world: &wp::World, measured: &HashMap<TypeId, Measure>) -> u64 {
    let id_size = |id: &TypeId| measured.get(id).map_or(LEAF.size, |m| m.size);
    world
        .imports
        .values()
        .chain(world.exports.values())
        .map(|item| match item {
            WorldItem::Interface { id, .. } => {
                let interface = &resolve.interfaces[*id];
                interface
                    .types
                    .values()
                    .map(id_size)
                    .chain(
                        interface
                            .functions
                            .values()
                            .map(|f| function_size(measured, f)),
                    )
                    .fold(1, u64::saturating_add)
            }
            WorldItem::Function(f) => function_size(measured, f),
            WorldItem::Type { id, .. } => id_size(id),
        })
        .fold(1, u64::saturating_add)
}

/// The canonical rendering of `fatal: func(message: string)`.
const FATAL_SIGNATURE: &str = r#"func("message":string)"#;

/// Checks the world's imports from the built-in `witgraph:runtime`
/// package: only `host@0.1.x`, exactly as every host provides it.
fn check_runtime_imports(resolve: &Resolve, world: &wp::World) -> Result<(), LowerErrorKind> {
    for item in world.imports.values() {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let interface = &resolve.interfaces[*id];
        if !is_witgraph_runtime(resolve, interface) {
            continue;
        }
        let id_string = interface_id(resolve, *id).unwrap_or_default();
        let fail = |reason: &str| {
            Err(LowerErrorKind::RuntimeImport {
                interface: id_string.clone(),
                reason: reason.to_string(),
            })
        };
        if interface.name.as_deref() != Some("host") {
            return fail("no such interface");
        }
        let version = interface
            .package
            .and_then(|p| resolve.packages[p].name.version.as_ref());
        let on_track =
            version.is_some_and(|v| wp::PackageName::version_compat_track_string(v) == "0.1");
        if !on_track {
            return fail("unsupported version");
        }
        let fatal_only = interface.types.is_empty()
            && interface.functions.len() == 1
            && interface.functions.get("fatal").is_some_and(|f| {
                !f.kind.is_async() && hash::encode_function(resolve, f) == FATAL_SIGNATURE
            });
        if !fatal_only {
            return fail("it differs from the built-in interface");
        }
    }
    Ok(())
}

/// The named types the ports reach, for editor metadata: every named type
/// in a port's type, and in those types' members, wherever it is declared,
/// in first-reference order. The well-known names themselves (`inputs`,
/// `outputs`) are left out, and so are types WAVE cannot represent (the
/// stream or future around a payload, say); their members are still
/// visited. The recursion is bounded by [`MAX_TYPE_DEPTH`], checked first.
fn named_types(
    resolve: &Resolve,
    ports: &[&wp::Type],
    well_known: &HashSet<TypeId>,
) -> Vec<NamedType> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // A pre-order walk with an explicit stack (an alias chain may be far
    // longer than types nest), members pushed in reverse so they come off
    // in declaration order.
    let mut stack: Vec<&wp::Type> = ports.iter().rev().copied().collect();
    while let Some(ty) = stack.pop() {
        let wp::Type::Id(id) = *ty else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        let def = &resolve.types[id];
        // `use iface.{t}` declares an alias named like its target: one entry.
        let reexport = matches!(
            def.kind,
            wp::TypeDefKind::Type(wp::Type::Id(target)) if resolve.types[target].name == def.name
        );
        if let Some(name) = &def.name
            && !well_known.contains(&id)
            && !reexport
            && let Ok(resolved) = resolve_wit_type(resolve, id)
        {
            // An alias's own doc comment, else the one of what it aliases.
            let docs = def.docs.contents.clone().or_else(|| {
                let defining = defining_id(resolve, ty).unwrap_or(id);
                resolve.types[defining].docs.contents.clone()
            });
            let owner = match def.owner {
                wp::TypeOwner::Interface(interface) => interface_id(resolve, interface),
                wp::TypeOwner::World(_) | wp::TypeOwner::None => None,
            };
            out.push(NamedType {
                name: name.clone(),
                owner,
                ty: resolved,
                docs,
            });
        }
        stack.extend(child_types(def).into_iter().rev());
    }
    out
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
/// so the primitive leaves are mapped here. What the payload may not
/// contain is found first, in one walk over the WIT type
/// ([`unsupported_payload`]).
fn payload_type(resolve: &Resolve, ty: &wp::Type) -> Result<Type, FieldErrorKind> {
    if let Some(error) = unsupported_payload(resolve, ty) {
        return Err(error);
    }
    let unsupported = |kind: String| FieldErrorKind::Unsupported { kind };
    Ok(match ty {
        wp::Type::Id(id) => {
            resolve_wit_type(resolve, *id).map_err(|e| unsupported(e.to_string()))?
        }
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
        wp::Type::ErrorContext => return Err(unsupported("error-context".into())),
    })
}

/// The first thing a payload type contains that a port cannot carry: a
/// `stream` or `future` ([`FieldErrorKind::NestedAsync`]: the top-level one
/// is already peeled off), or an `error-context`, resource, handle, `map`,
/// fixed-length list (whose length the contract hash cannot observe) or
/// unknown type. One walk over the type, each type id visited once.
fn unsupported_payload(resolve: &Resolve, ty: &wp::Type) -> Option<FieldErrorKind> {
    let unsupported = |kind: &str| FieldErrorKind::Unsupported { kind: kind.into() };
    let mut stack = vec![ty];
    let mut seen = HashSet::new();
    while let Some(ty) = stack.pop() {
        let id = match ty {
            wp::Type::ErrorContext => return Some(unsupported("error-context")),
            wp::Type::Id(id) => *id,
            _ => continue,
        };
        if !seen.insert(id) {
            continue;
        }
        let def = &resolve.types[id];
        match &def.kind {
            wp::TypeDefKind::Stream(_) | wp::TypeDefKind::Future(_) => {
                return Some(FieldErrorKind::NestedAsync);
            }
            kind @ (wp::TypeDefKind::Resource
            | wp::TypeDefKind::Handle(_)
            | wp::TypeDefKind::Map(..)
            | wp::TypeDefKind::FixedLengthList(..)
            | wp::TypeDefKind::Unknown) => return Some(unsupported(kind.as_str())),
            wp::TypeDefKind::Record(_)
            | wp::TypeDefKind::Tuple(_)
            | wp::TypeDefKind::Variant(_)
            | wp::TypeDefKind::Option(_)
            | wp::TypeDefKind::List(_)
            | wp::TypeDefKind::Type(_)
            | wp::TypeDefKind::Result(_)
            | wp::TypeDefKind::Flags(_)
            | wp::TypeDefKind::Enum(_) => stack.extend(child_types(def)),
        }
    }
    None
}

/// The record a well-known name resolves to, plus the id of its defining
/// type (the end of any alias chain).
fn expect_record<'a>(
    resolve: &'a Resolve,
    id: TypeId,
    name: &'static str,
) -> Result<(&'a wp::Record, TypeId), LowerErrorKind> {
    let id = dealias(resolve, id);
    match &resolve.types[id].kind {
        wp::TypeDefKind::Record(record) => Ok((record, id)),
        other => Err(LowerErrorKind::WellKnownNotARecord {
            record: name,
            found: other.as_str(),
        }),
    }
}

/// The defining type id of `ty`, following `type x = y` aliases.
fn defining_id(resolve: &Resolve, ty: &wp::Type) -> Option<TypeId> {
    type_id(ty).map(|id| dealias(resolve, id))
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

/// What the host must implement for the world: imported functions,
/// imported interfaces that carry functions or declare resources (a
/// resource needs a host implementation even with no methods), and
/// resources declared in the world itself. Type-only imports (bare types,
/// interfaces with neither) demand nothing of the host, and the built-in
/// `witgraph:runtime/host` interface is provided by every host (see
/// [`check_runtime_imports`]), so neither is a capability.
///
/// Each capability is named after what the component imports: a named
/// interface by its full id (`namespace:name/interface@version`), an
/// anonymous inline interface by its import name (`config`); the host links
/// either as an instance of that name. A bare function is its import name
/// prefixed `func:` (`func:blink`); the host links `blink` at the root. A
/// world resource `r` is `resource:r`, with its constructor, methods and
/// static functions as items, like an interface's resource; the host links
/// all of them at the root. None of these depends on the importing world's
/// package or name. Each capability also records its items' signatures
/// (see [`Capability::items`]).
fn capabilities(resolve: &Resolve, world: &wp::World) -> Vec<Capability> {
    let world_resource = |id: TypeId| -> Option<&String> {
        let def = &resolve.types[id];
        match (&def.kind, &def.owner) {
            (wp::TypeDefKind::Resource, wp::TypeOwner::World(_)) => def.name.as_ref(),
            _ => None,
        }
    };
    let mut capabilities: Vec<Capability> = Vec::new();
    let mut resources: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (key, item) in &world.imports {
        match item {
            WorldItem::Interface { id, .. } => {
                let interface = &resolve.interfaces[*id];
                if is_witgraph_runtime(resolve, interface) {
                    continue;
                }
                let declared = interface.types.iter().filter(|(_, ty)| {
                    matches!(resolve.types[**ty].kind, wp::TypeDefKind::Resource)
                });
                let items: BTreeMap<String, String> = declared
                    .map(|(name, _)| (name.clone(), "resource".to_string()))
                    .chain(interface.functions.iter().map(|(name, function)| {
                        (name.clone(), hash::encode_function(resolve, function))
                    }))
                    .collect();
                if !items.is_empty() {
                    capabilities.push(Capability {
                        interface: resolve.name_world_key(key),
                        items,
                    });
                }
            }
            WorldItem::Function(function) => {
                let signature = hash::encode_function(resolve, function);
                match function.kind.resource().and_then(world_resource) {
                    // A world resource's constructor, method or static.
                    Some(resource) => {
                        resources
                            .entry(resource.clone())
                            .or_default()
                            .insert(function.name.clone(), signature);
                    }
                    None => {
                        let name = resolve.name_world_key(key);
                        capabilities.push(Capability {
                            interface: format!("func:{name}"),
                            items: BTreeMap::from([(name, signature)]),
                        });
                    }
                }
            }
            WorldItem::Type { id, .. } => {
                if let Some(name) = world_resource(*id) {
                    resources
                        .entry(name.clone())
                        .or_default()
                        .insert(name.clone(), "resource".to_string());
                }
            }
        }
    }
    capabilities.extend(resources.into_iter().map(|(name, items)| Capability {
        interface: format!("resource:{name}"),
        items,
    }));
    capabilities.sort();
    capabilities
}

/// Whether `interface` is a named interface of the built-in
/// `witgraph:runtime` package. An anonymous inline interface is never one,
/// whatever package its world is declared in.
fn is_witgraph_runtime(resolve: &Resolve, interface: &wp::Interface) -> bool {
    interface.name.is_some()
        && interface.package.is_some_and(|package| {
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

    fn capability_names(contract: &ComponentContract) -> Vec<&str> {
        contract
            .capabilities
            .iter()
            .map(|c| c.interface.as_str())
            .collect()
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
        let Lowered {
            contract, types, ..
        } = lower_lowered(
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
            capability_names(&contract),
            ["demo:test/dep@0.1.0", "func:blink"],
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
            capability_names(&contract),
            ["demo:test/dep@0.1.0"],
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
        assert!(message.contains("world `demo:test/a@0.1.0`"), "{message}");
        assert!(message.contains("world `demo:test/b@0.1.0`"), "{message}");
    }

    #[test]
    fn failures_name_the_world_with_its_package() {
        let node = "export node: interface { record inputs { x: u32 } }";
        let wit = format!(
            "package demo:outer@0.1.0;
            package demo:inner@0.2.0 {{ world w {{ {node} }} }}
            world w {{ {node} }}"
        );
        let failures = lower(&load_str("test.wit", &wit).unwrap())
            .unwrap_err()
            .failures;
        let mut worlds: Vec<String> = failures.iter().map(|f| f.world.to_string()).collect();
        worlds.sort();
        assert_eq!(worlds, ["demo:inner/w@0.2.0", "demo:outer/w@0.1.0"]);
    }

    /// Whether wit-component encodes (and wasmparser validates) a component
    /// for world `w` of `wit`.
    fn encodes(wit: &str) -> bool {
        let mut resolve = Resolve {
            all_features: true,
            ..Resolve::default()
        };
        let package = resolve.push_str("test.wit", wit).unwrap();
        let world = resolve.select_world(&[package], Some("w")).unwrap();
        let mut module = wit_component::dummy_module(
            &resolve,
            world,
            wp::ManglingAndAbi::Legacy(wp::LiftLowerAbi::Sync),
        );
        wit_component::embed_component_metadata(
            &mut module,
            &resolve,
            world,
            wit_component::StringEncoding::UTF8,
        )
        .unwrap();
        wit_component::ComponentEncoder::default()
            .module(&module)
            .and_then(|encoder| encoder.encode())
            .is_ok()
    }

    /// Whether `wit` lowers, and whether a component encodes, agree.
    fn agree(wit: &str) -> bool {
        let lowers = lower_source(wit).is_ok();
        assert_eq!(
            lowers,
            encodes(wit),
            "lowering and encoding disagree on:\n{wit}"
        );
        lowers
    }

    #[test]
    fn nesting_is_limited_where_components_limit_it() {
        let nested = |levels: usize| {
            let mut ty = "u32".to_string();
            for _ in 0..levels {
                ty = format!("list<{ty}>");
            }
            format!(
                "package demo:test@0.1.0;
                world w {{ export node: interface {{ record outputs {{ out: {ty} }} run: func() -> outputs; }} }}"
            )
        };
        // `outputs` adds a level: 98 lists are 100 deep with it.
        assert!(agree(&nested(98)));
        assert!(!agree(&nested(99)));
        // Aliases add no depth.
        let mut aliases = String::from("type a0 = list<u32>;");
        for i in 1..150 {
            aliases.push_str(&format!(" type a{i} = a{};", i - 1));
        }
        let wit = format!(
            "package demo:test@0.1.0;
            world w {{ export node: interface {{ {aliases} record outputs {{ out: a149 }} run: func() -> outputs; }} }}"
        );
        assert!(agree(&wit));
    }

    #[test]
    fn effective_size_is_limited_in_linear_time() {
        // t{i} doubles t{i-1}: t{k} has size 2^(k+2) - 1.
        let doubling = |k: usize| {
            let mut types = String::from("type t0 = tuple<u8, u8>;");
            for i in 1..=k {
                types.push_str(&format!(" type t{i} = tuple<t{0}, t{0}>;", i - 1));
            }
            format!(
                "package demo:test@0.1.0;
                world w {{ export node: interface {{ {types} record outputs {{ out: t{k} }} run: func() -> outputs; }} }}"
            )
        };
        // A component embeds the whole world: t0..t{k} (about 2^(k+3)),
        // `outputs` and `run` (about 2^(k+2) each) together. That fits for
        // k = 15 and not for k = 16, though t16 alone would.
        assert!(agree(&doubling(15)));
        assert!(!agree(&doubling(16)));
        // Far past the limit, rejected at once: nothing is expanded.
        let started = std::time::Instant::now();
        let message = lower_err(&doubling(60));
        assert!(message.contains("effective size"), "{message}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn member_counts_are_limited_where_components_limit_them() {
        let with_enum = |cases: usize| {
            let cases: Vec<String> = (0..cases).map(|i| format!("c{i}")).collect();
            format!(
                "package demo:test@0.1.0;
                world w {{ export node: interface {{ enum e {{ {} }} record outputs {{ out: e }} run: func() -> outputs; }} }}",
                cases.join(", ")
            )
        };
        assert!(agree(&with_enum(10_000)));
        let message = lower_err(&with_enum(10_001));
        assert!(message.contains("10001 cases"), "{message}");
        assert!(!encodes(&with_enum(10_001)));
    }

    #[test]
    fn parameter_names_differing_only_in_case_are_rejected() {
        let wit = "package demo:test@0.1.0;
            interface caps { f: func(a: u32, A: u32); }
            world w {
                import caps;
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }";
        let message = lower_err(wit);
        assert!(message.contains("duplicate parameter `A`"), "{message}");
        assert!(!encodes(wit));
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
    fn named_types_are_the_ones_the_ports_reach() {
        let lowered = lower_lowered(
            r#"
            package demo:test@0.1.0;
            interface shapes {
                /// A point.
                record point { x: f32, y: f32 }
            }
            interface node {
                use shapes.{point};
                record io { x: u32 }
                record wrapper { at: point }
                type inputs = io;
                record outputs { y: io, w: list<wrapper> }
                record unused { z: u32 }
                run: func(inputs: inputs) -> outputs;
            }
            world w { export node; }
            "#,
        );
        let names: Vec<&str> = lowered.types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["io", "wrapper", "point"],
            "`io` reached through `outputs`, `point` from another interface, no `unused`"
        );
        assert_eq!(lowered.types[2].docs.as_deref(), Some("A point."));
        assert_eq!(
            lowered.types[2].owner.as_deref(),
            Some("demo:test/shapes@0.1.0")
        );
        assert_eq!(
            lowered.types[0].owner.as_deref(),
            Some("demo:test/node@0.1.0")
        );
    }

    #[test]
    fn names_differing_only_in_case_are_rejected() {
        let interface = lower_err(
            r#"
            package demo:test@0.1.0;
            interface caps { get: func(); GET: func(); }
            world w {
                import caps;
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            "#,
        );
        assert!(
            interface.contains("duplicate function or type `GET`"),
            "{interface}"
        );
    }

    #[test]
    fn world_resources_are_capabilities() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            world w {
                resource r;
                export node: interface {
                    record outputs { out: u32 }
                    run: func() -> outputs;
                }
            }
            "#,
        );
        assert_eq!(capability_names(&contract), ["resource:r"]);
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
    fn inline_interface_capability_is_its_import_name() {
        let wit = |package: &str, world: &str| {
            format!(
                r#"
                package {package};
                interface node {{
                    record outputs {{ out: u32 }}
                    run: func() -> outputs;
                }}
                world {world} {{
                    import config: interface {{ get: func() -> u32; }}
                    export node;
                }}
                "#
            )
        };
        let contract = lower_one(&wit("demo:test@0.1.0", "w"));
        assert_eq!(
            capability_names(&contract),
            ["config"],
            "named as the component imports it"
        );
        let elsewhere = lower_one(&wit("other:pkg@2.0.0", "renamed"));
        assert_eq!(
            contract.id.content_hash, elsewhere.id.content_hash,
            "the package, world and version stay out of the hash"
        );
    }

    #[test]
    fn capability_signatures_are_part_of_the_contract() {
        let wit = |result: &str| {
            format!(
                r#"
                package demo:test@0.1.0;
                world w {{
                    import config: interface {{ get: func() -> {result}; }}
                    import blink: func(times: u8);
                    export node: interface {{
                        record outputs {{ out: u32 }}
                        run: func() -> outputs;
                    }}
                }}
                "#
            )
        };
        let a = lower_one(&wit("u32"));
        assert_eq!(a.capabilities[0].items["get"], "func()->u32");
        assert_eq!(a.capabilities[1].items["blink"], r#"func("times":u8)"#);
        let b = lower_one(&wit("string"));
        assert_ne!(
            a.id.content_hash, b.id.content_hash,
            "a capability signature change is a different contract"
        );
    }

    #[test]
    fn interfaces_declaring_resources_are_capabilities() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface handles { resource token; }
            interface uses-handles { use handles.{token}; }
            world w {
                import handles;
                import uses-handles;
                export node: interface {
                    record outputs { out: u32 }
                    run: func() -> outputs;
                }
            }
            "#,
        );
        assert_eq!(
            capability_names(&contract),
            ["demo:test/handles@0.1.0"],
            "a resource needs a host implementation; a `use` of one does not"
        );
    }

    #[test]
    fn labelled_interfaces_are_rejected() {
        let import = lower_err(
            r#"
            package demo:test@0.1.0;
            interface clock { now: func() -> u64; }
            world w {
                import primary: clock;
                export node: interface {
                    record outputs { out: u32 }
                    run: func() -> outputs;
                }
            }
            "#,
        );
        assert!(
            import.contains("import `primary` gives interface `clock` a label"),
            "{import}"
        );
        let export = lower_err(
            r#"
            package demo:test@0.1.0;
            interface contract {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
            world w { export node: contract; }
            "#,
        );
        assert!(
            export.contains("export `node` gives interface `contract` a label"),
            "{export}"
        );
    }

    #[test]
    fn a_node_exported_under_a_label_is_reported() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
            world w { export primary: node; }
            "#,
        );
        assert!(
            message.contains("export `primary` gives interface `node` a label"),
            "{message}"
        );
    }

    #[test]
    fn external_ids_are_rejected() {
        for (attributed, name) in [
            (
                r#"@external-id("abc") export node: interface { record outputs { out: u32 } run: func() -> outputs; }"#,
                "node",
            ),
            (
                r#"export node: interface { record outputs { out: u32 } @external-id("abc") run: func() -> outputs; }"#,
                "run",
            ),
        ] {
            let message = lower_err(&format!(
                "package demo:test@0.1.0; world w {{ {attributed} }}"
            ));
            assert!(
                message.contains(&format!("`{name}` carries `@external-id`")),
                "{message}"
            );
        }
    }

    #[test]
    fn the_export_name_is_what_a_component_exports() {
        let named = lower_lowered(
            r#"
            package demo:test@0.1.0;
            interface node {
                record outputs { out: u32 }
                run: func() -> outputs;
            }
            world w { export node; }
            "#,
        );
        assert_eq!(named.export, "demo:test/node@0.1.0");
        let inline = lower_lowered(
            r#"
            package demo:test@0.1.0;
            world w {
                export node: interface {
                    record outputs { out: u32 }
                    run: func() -> outputs;
                }
            }
            "#,
        );
        assert_eq!(inline.export, "node");
    }

    /// `type t0 = list<u32>; type t1 = list<t0>; ...` up to `t{n-1}`, as
    /// the type of a port.
    fn nested_lists(n: usize) -> String {
        let mut types = String::from("type t0 = list<u32>;\n");
        for i in 1..n {
            types.push_str(&format!("type t{i} = list<t{}>;\n", i - 1));
        }
        format!(
            "package demo:deep@0.1.0;
            interface node {{
                {types}
                record inputs {{ x: t{} }}
                run: func(inputs: inputs);
            }}
            world w {{ export node; }}",
            n - 1
        )
    }

    #[test]
    fn types_nested_past_the_component_limit_are_rejected() {
        let message = lower_err(&nested_lists(120));
        assert!(message.contains("levels deep"), "{message}");
        lower_one(&nested_lists(50));
    }

    #[test]
    fn a_very_deep_alias_chain_is_an_error_not_a_stack_overflow() {
        let wit = nested_lists(20_000);
        let lowered = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let source = load_str("deep.wit", &wit).expect("wit-parser accepts it");
                lower(&source).map(|l| l.len())
            })
            .unwrap()
            .join()
            .expect("no stack overflow");
        let message = format!("{:#}", lowered.unwrap_err());
        assert!(message.contains("levels deep"), "{message}");
    }

    #[test]
    fn a_long_alias_chain_lowers_without_deep_recursion() {
        let mut aliases = String::from("type a0 = u32;");
        for i in 1..20_000 {
            aliases.push_str(&format!(" type a{i} = a{};", i - 1));
        }
        let wit = format!(
            "package demo:test@0.1.0;
            interface caps {{ {aliases} get: func(x: a19999) -> a19999; }}
            world w {{
                import caps;
                export node: interface {{ use caps.{{a19999}}; record outputs {{ out: a19999 }} run: func() -> outputs; }}
            }}"
        );
        // wit-parser itself recurses along the chain: parse on a large
        // stack, then lower on a small one.
        let source = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || load_str("aliases.wit", &wit).expect("wit-parser accepts it"))
            .unwrap()
            .join()
            .unwrap();
        let lowered = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || lower(&source).map(|l| l.len()))
            .unwrap()
            .join()
            .expect("no stack overflow");
        assert_eq!(lowered, Ok(1), "aliases add no depth");
    }

    #[test]
    fn a_nested_error_context_is_named_as_such() {
        let message = lower_err(
            r#"
            package demo:test@0.1.0;
            world w {
                export node: interface {
                    record outputs { out: list<error-context> }
                    run: func() -> outputs;
                }
            }
            "#,
        );
        assert!(message.contains("`error-context` types"), "{message}");
    }

    #[test]
    fn only_the_built_in_host_interface_may_be_imported_from_witgraph_runtime() {
        let wit = |runtime: &str, import: &str| {
            format!(
                r#"
                package demo:test@0.1.0;
                package witgraph:runtime@{runtime} {{
                    interface host {{ fatal: func(message: string); }}
                    interface log {{ write: func(line: string); }}
                    interface odd {{ fatal: func(code: u32); }}
                }}
                world w {{
                    import {import};
                    export node: interface {{ record outputs {{ out: u32 }} run: func() -> outputs; }}
                }}
                "#
            )
        };
        let ok = lower_one(&wit("0.1.3", "witgraph:runtime/host@0.1.3"));
        assert!(ok.capabilities.is_empty(), "the host interface is built in");
        for (version, import, expected) in [
            ("0.1.0", "witgraph:runtime/log@0.1.0", "no such interface"),
            (
                "0.2.0",
                "witgraph:runtime/host@0.2.0",
                "unsupported version",
            ),
            ("0.1.0", "witgraph:runtime/odd@0.1.0", "no such interface"),
        ] {
            let message = lower_err(&wit(version, import));
            assert!(message.contains(expected), "{import}: {message}");
        }
        let changed = lower_err(
            r#"
            package demo:test@0.1.0;
            package witgraph:runtime@0.1.0 {
                interface host { fatal: func(code: u32); }
            }
            world w {
                import witgraph:runtime/host@0.1.0;
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            "#,
        );
        assert!(changed.contains("differs from the built-in"), "{changed}");
    }

    #[test]
    fn an_inline_import_in_the_runtime_package_is_an_ordinary_capability() {
        let contract = lower_one(
            r#"
            package witgraph:runtime@0.1.0;
            world w {
                import config: interface { get: func() -> u32; }
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            "#,
        );
        assert_eq!(capability_names(&contract), ["config"]);
    }

    #[test]
    fn resources_keep_their_identity_only_on_one_semver_track() {
        let signature = |a: &str, b: &str, used: &str| {
            let contract = lower_one(&format!(
                r#"
                package demo:app@0.1.0;
                package demo:caps@{a} {{ interface clock {{ resource timer; }} }}
                package demo:caps@{b} {{ interface clock {{ resource timer; }} }}
                interface api {{
                    use demo:caps/clock@{used}.{{timer}};
                    start: func() -> timer;
                }}
                world w {{
                    import api;
                    export node: interface {{ record outputs {{ out: u32 }} run: func() -> outputs; }}
                }}
                "#
            ));
            contract
                .capabilities
                .iter()
                .find(|c| c.interface.contains("/api"))
                .unwrap()
                .items["start"]
                .clone()
        };
        assert_eq!(
            signature("0.2.0", "0.2.3", "0.2.0"),
            signature("0.2.0", "0.2.3", "0.2.3"),
            "one track: one resource"
        );
        assert_ne!(
            signature("1.0.0", "2.0.0", "1.0.0"),
            signature("1.0.0", "2.0.0", "2.0.0"),
            "two tracks: two resources"
        );
    }

    #[test]
    fn types_no_world_reaches_are_not_checked() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            interface unused { record nothing {} }
            world w {
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            "#,
        );
        assert!(contract.inputs.is_empty());
    }

    #[test]
    fn lower_each_keeps_the_worlds_that_lower() {
        let source = load_str(
            "t.wit",
            r#"
            package demo:test@0.1.0;
            world good {
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            world bad {
                export node: interface { record outputs { out: u32 } }
            }
            "#,
        )
        .unwrap();
        let results = lower_each(&source);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .any(|r| r.as_ref().is_ok_and(|l| l.contract.id.world == "good"))
        );
        assert!(
            results
                .iter()
                .any(|r| r.as_ref().is_err_and(|e| e.world.world == "bad"))
        );
        assert!(lower(&source).is_err(), "`lower` still fails as a whole");
    }

    #[test]
    fn a_world_resource_carries_its_methods_as_items() {
        let contract = lower_one(
            r#"
            package demo:test@0.1.0;
            world w {
                resource r {
                    constructor(x: u32);
                    get: func() -> u32;
                    make: static func() -> r;
                }
                export node: interface { record outputs { out: u32 } run: func() -> outputs; }
            }
            "#,
        );
        assert_eq!(capability_names(&contract), ["resource:r"]);
        let items: Vec<&str> = contract.capabilities[0]
            .items
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            items,
            ["[constructor]r", "[method]r.get", "[static]r.make", "r"]
        );
    }

    #[test]
    fn repeated_member_names_are_rejected() {
        for (decl, expected) in [
            (
                "enum mode { a, a }",
                "enum `mode` in `demo:test/node@0.1.0` with duplicate case `a`",
            ),
            (
                "flags perms { r, r }",
                "flags `perms` in `demo:test/node@0.1.0` with duplicate flag `r`",
            ),
            (
                "variant v { a, a(u32) }",
                "variant `v` in `demo:test/node@0.1.0` with duplicate case `a`",
            ),
            (
                "record pair { x: u32, x: u8 }",
                "record `pair` in `demo:test/node@0.1.0` with duplicate field `x`",
            ),
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
            assert!(message.contains(expected), "{message}");
        }
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
        assert_eq!(capability_names(&contract), ["config"]);
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
