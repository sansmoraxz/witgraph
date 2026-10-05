//! Loading: checking component bytes against their contracts, compiling
//! and linking them, and wiring the islands.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use futures::channel::mpsc;
use wasmtime::Engine;
use wasmtime::component::{Component, Type};
use witgraph_ir::{
    CompiledGraph, ComponentContract, ComponentRef, Connection, ConnectionId, NodeId, PortDef,
    PortKind, PortRef, ResourceId,
};

use super::run::{IslandWaker, WakeSet};
use super::{Edge, Route, RuntimeConfig, RuntimeGraph, Slot};
use crate::engine::{
    self, BuildError, Consumer, Host, HostState, InputField, InputSource, IslandPlan, MemberPlan,
    NoCapabilities, NodeBinary, OutputField, RunSignature,
};
use crate::error::RuntimeError;
use crate::island::{Island, Owed};
use crate::mode::RuntimeMode;
use crate::resource::ResourcePool;

/// A component checked against its contract and compiled, ready for
/// [`RuntimeGraph::load_prepared`].
///
/// Preparing is the slow part of a load: it validates the bytes, decodes
/// and checks their WIT, and compiles them with Cranelift, which can take
/// seconds. It runs synchronously, so an embedder can run it off its async
/// executor (`spawn_blocking`, say) and keep the result to load the same
/// component again without recompiling. A prepared component belongs to
/// the engine it was compiled on, and to the contract it was checked
/// against.
#[derive(Clone)]
pub struct PreparedComponent {
    contract: ComponentContract,
    component: Component,
    /// The name the component exports its `node` interface under.
    export: String,
}

impl PreparedComponent {
    /// Checks `bytes` against `contract` and compiles them on `engine`.
    ///
    /// The WIT embedded in the bytes is decoded and lowered, and must
    /// describe the same `run` kind and ports as `contract`, and import no
    /// capability item the contract does not declare with the same
    /// signature ([`RuntimeError::ContractMismatch`]). Bytes that are not a
    /// valid component, whose WIT cannot be decoded, or that fail to
    /// compile on `engine` are a [`RuntimeError::BadComponent`].
    pub fn new(
        engine: &Engine,
        contract: &ComponentContract,
        bytes: &[u8],
    ) -> Result<Self, RuntimeError> {
        let bad = |message: String| RuntimeError::BadComponent {
            component: Box::new(contract.id.clone()),
            message,
        };
        check_decodable(bytes).map_err(bad)?;
        let export = verify(contract, bytes)?;
        let component =
            Component::new(engine, bytes).map_err(|e| bad(format!("failed to compile: {e:#}")))?;
        Ok(Self {
            contract: contract.clone(),
            component,
            export,
        })
    }

    /// The contract the component was checked against.
    pub fn contract(&self) -> &ComponentContract {
        &self.contract
    }

    /// The engine the component was compiled on.
    pub fn engine(&self) -> &Engine {
        self.component.engine()
    }
}

impl<M: RuntimeMode> RuntimeGraph<M, NoCapabilities> {
    /// Loads a compiled graph whose components import no capabilities; see
    /// [`load_with_host`](RuntimeGraph::load_with_host).
    pub async fn load(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
    ) -> Result<Self, RuntimeError> {
        Self::load_with_host(compiled, wasm, config, mode, NoCapabilities).await
    }
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Loads a compiled graph with WASM component bytes, with `host`
    /// providing capability imports and island Store data.
    ///
    /// `wasm` maps each component a node instantiates (by the
    /// [`ComponentRef`] of its resolved contract, or the same ref without a
    /// content hash) to its encoded component bytes. Each is prepared
    /// ([`PreparedComponent::new`]) on [`RuntimeConfig::engine`] (or a new
    /// engine), then loaded as [`load_prepared`](Self::load_prepared) does.
    ///
    /// Preparing compiles every component *synchronously on the task that
    /// polls this future*, which blocks its executor thread for as long as
    /// compiling takes (often seconds per component). On a shared async
    /// executor, prepare the components with [`PreparedComponent::new`] on
    /// a blocking thread instead, and call `load_prepared`.
    pub async fn load_with_host(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
        host: H,
    ) -> Result<Self, RuntimeError> {
        validate(&config)?;
        let engine = match &config.engine {
            Some(engine) => engine.clone(),
            None => config.new_engine()?,
        };
        validate_engine(&engine)?;
        let mut prepared = Vec::new();
        let mut seen = HashSet::new();
        for node in &compiled.graph().nodes {
            let contract = contract_of(&compiled, &node.id)?;
            if !seen.insert(&contract.id) {
                continue;
            }
            let bytes =
                find_bytes(wasm, &contract.id).ok_or_else(|| RuntimeError::MissingWasm {
                    component: Box::new(contract.id.clone()),
                })?;
            prepared.push(PreparedComponent::new(&engine, contract, bytes)?);
        }
        Self::link_and_build(compiled, &prepared, config, engine, mode, host).await
    }

    /// Loads a compiled graph from prepared components (one per component a
    /// node instantiates, matched by its resolved contract), with `host`
    /// providing capability imports and island Store data.
    ///
    /// The engine is [`RuntimeConfig::engine`], or else the one the
    /// components were prepared on; every component must have been
    /// prepared on it ([`RuntimeError::InvalidConfig`]), against the very
    /// contract its nodes resolved to ([`RuntimeError::ContractMismatch`]).
    /// Each node is linked once ([`Host::link`]), and every island is
    /// instantiated.
    pub async fn load_prepared(
        compiled: CompiledGraph,
        prepared: &[PreparedComponent],
        config: RuntimeConfig,
        mode: M,
        host: H,
    ) -> Result<Self, RuntimeError> {
        validate(&config)?;
        let engine = match (&config.engine, prepared.first()) {
            (Some(engine), _) => engine.clone(),
            (None, Some(first)) => first.engine().clone(),
            (None, None) => config.new_engine()?,
        };
        validate_engine(&engine)?;
        if let Some(other) = prepared.iter().find(|p| !Engine::same(p.engine(), &engine)) {
            return Err(RuntimeError::InvalidConfig {
                message: format!("`{}` was prepared on another engine", other.contract.id),
            });
        }
        Self::link_and_build(compiled, prepared, config, engine, mode, host).await
    }

    /// Links every node and builds every island, with a checked config and
    /// engine.
    async fn link_and_build(
        compiled: CompiledGraph,
        prepared: &[PreparedComponent],
        config: RuntimeConfig,
        engine: Engine,
        mode: M,
        host: H,
    ) -> Result<Self, RuntimeError> {
        let resources = register_resources(&compiled)?;
        let config = RuntimeConfig {
            engine: Some(engine.clone()),
            ..config
        };
        let by_id: HashMap<&ComponentRef, &PreparedComponent> =
            prepared.iter().map(|p| (&p.contract.id, p)).collect();
        let settings = config.store_settings();

        let graph = compiled.graph();
        let mut binaries = HashMap::new();
        for node in &graph.nodes {
            let contract = contract_of(&compiled, &node.id)?;
            let prepared = by_id
                .get(&contract.id)
                .ok_or_else(|| RuntimeError::MissingWasm {
                    component: Box::new(contract.id.clone()),
                })?;
            if prepared.contract != *contract {
                return Err(RuntimeError::ContractMismatch {
                    component: Box::new(contract.id.clone()),
                    message: "it was prepared against another contract with the same id".into(),
                });
            }
            let instantiation = |e: wasmtime::Error| RuntimeError::Instantiation {
                node: node.id.clone(),
                message: format!("{e:#}"),
            };
            let linker = engine::node_linker(&engine, &node.id, contract, &host)
                .map_err(|e| instantiation(e.context("linker")))?;
            let pre = linker
                .instantiate_pre(&prepared.component)
                .map_err(instantiation)?;
            binaries.insert(
                node.id.clone(),
                Arc::new(NodeBinary {
                    pre,
                    export: prepared.export.clone(),
                }),
            );
        }

        let wiring = Wiring::new(&compiled);
        let mut out_edges: HashMap<PortRef, Vec<Edge>> = HashMap::new();
        let mut feedback_edges = HashMap::new();
        let mut feedback_into: HashMap<PortRef, Vec<(ConnectionId, usize)>> = HashMap::new();
        let island_of: Vec<usize> = compiled.nodes().map(|n| n.island).collect();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (from, to) = (island_of[resolved.from_node], island_of[resolved.to_node]);
            let route = match (conn.feedback, from == to) {
                (true, _) => Route::Feedback,
                (false, true) => Route::Internal,
                (false, false) => Route::External,
            };
            let edge = Edge {
                id: conn.id.clone(),
                to: conn.to.clone(),
                route,
                unwrap_option: resolved.unwraps_option,
            };
            if conn.feedback {
                feedback_edges.insert(conn.id.clone(), (conn.from.clone(), edge.clone()));
                feedback_into
                    .entry(conn.to.clone())
                    .or_default()
                    .push((conn.id.clone(), from));
            }
            out_edges.entry(conn.from.clone()).or_default().push(edge);
        }
        let out_edges = out_edges
            .into_iter()
            .map(|(port, edges)| (port, edges.into()))
            .collect();
        let mut connections = graph.connections.clone();
        connections.sort_by(|a, b| a.id.cmp(&b.id));
        let components = graph
            .nodes
            .iter()
            .filter_map(|n| Some((n.id.clone(), compiled.contract_for(&n.id)?.id.clone())))
            .collect();
        let connected = wiring
            .writer
            .iter()
            .map(|(port, conn)| ((*port).clone(), conn.id.clone()))
            .collect();

        let mut islands = Vec::new();
        let mut input_types = HashMap::new();
        let mut output_types = HashMap::new();
        for (index, members) in compiled.islands().iter().enumerate() {
            let parts = island_parts(&binaries, members)?;
            let first = || members.first().cloned().unwrap_or_else(|| "?".into());
            let state = HostState::new(config.max_island_memory, members.len());
            let data =
                host.island_data(members, state)
                    .map_err(|e| RuntimeError::Instantiation {
                        node: first(),
                        message: format!("island data: {e:#}"),
                    })?;
            let (island, signatures) = engine::build_island(&engine, &parts, data, settings)
                .await
                .map_err(
                    |BuildError { node, message, .. }| RuntimeError::Instantiation {
                        node,
                        message,
                    },
                )?;
            let plan = plan_island(
                &compiled,
                &wiring,
                index,
                members,
                &signatures,
                &mut input_types,
                &mut output_types,
            )?;
            islands.push((plan, island));
        }

        let wired = node_wiring(&compiled, &wiring);
        let wakes = Arc::new(WakeSet::new(islands.len()));
        let slots: Vec<Slot<H::Data>> = islands
            .into_iter()
            .zip(wired)
            .enumerate()
            .map(|(index, ((plan, store), wired))| {
                let required = required_inputs(&plan);
                let members = plan.members.len();
                let island_binaries = plan
                    .members
                    .iter()
                    .filter_map(|m| binaries.get(&m.node).cloned())
                    .collect();
                // A new island owes its first generation.
                let mut owed = Owed::default();
                owed.push_latched();
                Slot {
                    plan: Arc::new(plan),
                    binaries: island_binaries,
                    offset: wired.offset,
                    preds: wired.preds,
                    feedback_sources: wired.feedback_sources,
                    feedback_out: wired.feedback_out,
                    required,
                    awaiting: vec![false; members],
                    stale_feedback: BTreeSet::new(),
                    waker: futures::task::waker(Arc::new(IslandWaker {
                        island: index,
                        set: wakes.clone(),
                    })),
                    state: Island::new(store).into(),
                    owed,
                }
            })
            .collect();

        let island_index = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| (sorted_members(&slot.plan), index))
            .collect();
        let (events_tx, events_rx) = mpsc::unbounded();
        Ok(Self {
            compiled,
            config,
            mode,
            host,
            engine,
            settings,
            slots,
            running: 0,
            island_index,
            inputs: HashMap::new(),
            input_types,
            output_types,
            outputs: HashMap::new(),
            out_edges,
            feedback_edges,
            feedback_into,
            connections,
            components,
            connected,
            feedback: BTreeMap::new(),
            faults: BTreeMap::new(),
            wakes,
            dirty: true,
            events_tx,
            events_rx,
            resources,
        })
    }
}

/// Every island's summed resource claims. Compilation already rejects an
/// island whose own claims on a resource exceed all of it
/// (`Diagnostic::IslandOverclaims`); one could never start, so it stays an
/// error here ([`RuntimeError::InvalidConfig`]).
fn register_resources(compiled: &CompiledGraph) -> Result<ResourcePool, RuntimeError> {
    let mut resources = ResourcePool::default();
    let nodes: HashMap<&NodeId, &witgraph_ir::Node> =
        compiled.graph().nodes.iter().map(|n| (&n.id, n)).collect();
    for (index, members) in compiled.islands().iter().enumerate() {
        let mut claims: BTreeMap<ResourceId, f64> = BTreeMap::new();
        for node in members.iter().filter_map(|m| nodes.get(m)) {
            for (resource, claim) in &node.resources {
                *claims.entry(resource.clone()).or_insert(0.0) += claim.fraction.get();
            }
        }
        resources
            .register(index, claims)
            .map_err(|resource| RuntimeError::InvalidConfig {
                message: format!(
                    "the island of `{}` claims more than all of resource `{resource}`",
                    members.first().map_or("?", |n| n.as_str())
                ),
            })?;
    }
    Ok(resources)
}

pub(super) fn validate(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    let invalid = |message: &str| {
        Err(RuntimeError::InvalidConfig {
            message: message.into(),
        })
    };
    if config.max_steps_per_tick == 0 {
        return invalid("max_steps_per_tick must be at least 1");
    }
    if config.yield_interval == 0 {
        return invalid("yield_interval must be at least 1");
    }
    if config.fuel_per_run == Some(0) {
        return invalid("fuel_per_run must be at least 1");
    }
    if config.max_island_memory == Some(0) {
        return invalid("max_island_memory must be at least 1");
    }
    if config.hostcall_fuel == 0 {
        return invalid("hostcall_fuel must be at least 1");
    }
    if let Some(bytes) = config.memory_reservation
        && bytes.checked_next_multiple_of(64 * 1024).is_none()
    {
        return invalid("memory_reservation is too large to round to whole 64 KiB pages");
    }
    Ok(())
}

/// Checks that an engine (maybe the embedder's) can run a graph: fuel
/// metering, the component model with its async ABI, concurrency and every
/// value type lowering admits in a capability signature (`map`,
/// `error-context`, fixed-length lists); no epoch interruption (no deadline
/// is ever set, so every call would trap); and no shared memories, whose
/// growth the island memory limit never sees.
fn validate_engine(engine: &Engine) -> Result<(), RuntimeError> {
    use wasmtime::WasmFeatures;
    let features = engine.get_wasm_features();
    let missing = [
        (engine.get_consume_fuel(), "fuel metering (`consume_fuel`)"),
        (
            engine.get_concurrency_support(),
            "concurrency (`concurrency_support`)",
        ),
        (
            features.contains(WasmFeatures::COMPONENT_MODEL),
            "the component model",
        ),
        (
            features.contains(WasmFeatures::CM_ASYNC),
            "the component model's async ABI",
        ),
        (
            features.contains(WasmFeatures::CM_MAP),
            "the component model's `map` type",
        ),
        (
            features.contains(WasmFeatures::CM_ERROR_CONTEXT),
            "the component model's `error-context` type",
        ),
        (
            features.contains(WasmFeatures::CM_FIXED_LENGTH_LISTS),
            "the component model's fixed-length lists",
        ),
        (
            !engine.get_epoch_interruption(),
            "no epoch interruption (witgraph sets no deadline)",
        ),
        (
            !engine.get_shared_memory(),
            "no shared memories (the island memory limit cannot see their growth)",
        ),
    ];
    match missing.iter().find(|(ok, _)| !ok) {
        Some((_, what)) => Err(RuntimeError::InvalidConfig {
            message: format!("the engine lacks {what}; use `RuntimeConfig::new_engine`"),
        }),
        None => Ok(()),
    }
}

pub(super) fn contract_of<'a>(
    compiled: &'a CompiledGraph,
    node: &NodeId,
) -> Result<&'a ComponentContract, RuntimeError> {
    compiled
        .contract_for(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
}

/// The bytes for a component: keyed by its full ref, or by a ref without
/// a content hash that matches it (same package, version included, and
/// world).
fn find_bytes<'a>(wasm: &HashMap<ComponentRef, &'a [u8]>, id: &ComponentRef) -> Option<&'a [u8]> {
    wasm.get(id).copied().or_else(|| {
        wasm.iter()
            .find(|(key, _)| key.content_hash.is_none() && key.matches(id))
            .map(|(_, bytes)| *bytes)
    })
}

/// Validates untrusted bytes as a component, and rejects what wit-parser's
/// decoder cannot represent: a type imported or exported (at any depth)
/// that is not a value or resource type, a nested component, or a core
/// module. wit-parser panics on some of those, and `catch_unwind` cannot
/// contain a panic in a host built with `panic = "abort"`.
fn check_decodable(bytes: &[u8]) -> Result<(), String> {
    use wasmparser::component_types::{ComponentAnyTypeId, ComponentEntityType};
    use wasmparser::{Parser, Payload, ValidPayload, Validator, WasmFeatures};

    fn check(
        ty: &ComponentEntityType,
        types: wasmparser::types::TypesRef<'_>,
    ) -> Result<(), String> {
        match ty {
            ComponentEntityType::Func(_) | ComponentEntityType::Value(_) => Ok(()),
            ComponentEntityType::Type { referenced, .. } => match referenced {
                ComponentAnyTypeId::Defined(_) | ComponentAnyTypeId::Resource(_) => Ok(()),
                _ => Err("it imports or exports a component, instance or function type".into()),
            },
            ComponentEntityType::Instance(id) => types[*id]
                .exports
                .values()
                .try_for_each(|item| check(&item.ty, types)),
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
    for name in imports {
        if let Some(item) = types.component_item_for_import(name) {
            check(&item.ty, types).map_err(|e| format!("import `{name}`: {e}"))?;
        }
    }
    for name in exports {
        if let Some(item) = types.component_item_for_export(name) {
            check(&item.ty, types).map_err(|e| format!("export `{name}`: {e}"))?;
        }
    }
    Ok(())
}

/// Decodes the WIT embedded in a component, lowers it, and checks that the
/// component implements `contract`: the same `run` kind and ports, and no
/// capability import the contract does not declare. Returns the name the
/// component exports its `node` interface under.
///
/// The capabilities are a subset check, item by item, not an equality: a
/// component built from the contract's world imports only the functions
/// and resources its code uses, and an interface pulled in only for its
/// types imports none. Every item it does import must have the signature
/// the contract declares.
fn verify(contract: &ComponentContract, bytes: &[u8]) -> Result<String, RuntimeError> {
    let bad = |message: String| RuntimeError::BadComponent {
        component: Box::new(contract.id.clone()),
        message,
    };
    let mismatch = |message: String| RuntimeError::ContractMismatch {
        component: Box::new(contract.id.clone()),
        message,
    };
    // `check_decodable` rules out the inputs wit-parser's decoder is known
    // to panic on; anything else it panics on is still contained here
    // (where panics unwind).
    let lowered = std::panic::catch_unwind(|| {
        let decoded = wit_parser::decoding::decode(bytes).map_err(|e| format!("{e:#}"))?;
        let package = decoded.package();
        let wit_parser::decoding::DecodedWasm::Component(resolve, _) = decoded else {
            return Err("the bytes are a WIT package, not a component".to_string());
        };
        let source = witgraph_wit::load::WitSource {
            resolve,
            packages: vec![package],
        };
        witgraph_wit::lower::lower(&source).map_err(|e| e.to_string())
    })
    .map_err(|_| bad("the component's WIT could not be decoded".into()))?
    .map_err(bad)?;
    let [found] = lowered.as_slice() else {
        return Err(bad(format!(
            "expected exactly one node world, found {}",
            lowered.len()
        )));
    };
    if found.contract.run != contract.run {
        return Err(mismatch(format!(
            "`run` is {} in the contract but {} in the bytes",
            contract.run, found.contract.run
        )));
    }
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
        let declared = contract
            .capabilities
            .iter()
            .find(|c| c.interface == capability.interface)
            .or_else(|| {
                contract
                    .capabilities
                    .iter()
                    .filter(|c| semver_compatible(&c.interface, &capability.interface))
                    .max_by_key(|c| interface_version(&c.interface))
            });
        let Some(declared) = declared else {
            return Err(mismatch(format!(
                "the bytes import `{}`, which the contract does not declare",
                capability.interface
            )));
        };
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
    Ok(found.export.clone())
}

/// An interface id split into the id without its version, and the
/// version, if it has one.
fn split_version(id: &str) -> Option<(&str, semver::Version)> {
    let (base, version) = id.rsplit_once('@')?;
    Some((base, semver::Version::parse(version).ok()?))
}

/// The version of an interface id, if it has one.
fn interface_version(id: &str) -> Option<semver::Version> {
    split_version(id).map(|(_, version)| version)
}

/// Whether two interface ids name the same interface at semver-compatible
/// versions, the way wasmtime's linker matches imports: on the same
/// compatibility track (the same major from 1.0, the same minor for 0.x).
/// A 0.0.x or pre-release version only matches itself, exactly (as
/// wasmtime's linker does).
pub(super) fn semver_compatible(a: &str, b: &str) -> bool {
    let (Some((base_a, va)), Some((base_b, vb))) = (split_version(a), split_version(b)) else {
        return false;
    };
    let exact_only = |v: &semver::Version| (v.major == 0 && v.minor == 0) || !v.pre.is_empty();
    if base_a != base_b || exact_only(&va) || exact_only(&vb) {
        return false;
    }
    wit_parser::PackageName::version_compat_track(&va)
        == wit_parser::PackageName::version_compat_track(&vb)
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

/// Pairs each member of an island with what instantiating it needs, in
/// island order.
pub(super) fn island_parts<'a, D>(
    binaries: &'a HashMap<NodeId, Arc<NodeBinary<D>>>,
    members: &'a [NodeId],
) -> Result<Vec<(&'a NodeId, &'a NodeBinary<D>)>, RuntimeError> {
    members
        .iter()
        .map(|node| {
            binaries
                .get(node)
                .map(|binary| (node, &**binary))
                .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
        })
        .collect()
}

/// An island's members, sorted: how snapshots name an island.
pub(super) fn sorted_members(plan: &IslandPlan) -> Vec<NodeId> {
    let mut members: Vec<NodeId> = plan.members.iter().map(|m| m.node.clone()).collect();
    members.sort();
    members
}

/// Every member's required external Value inputs.
fn required_inputs(plan: &IslandPlan) -> Vec<PortRef> {
    plan.members
        .iter()
        .flat_map(|member| {
            member
                .external_values()
                .filter(|(_, field)| !field.optional)
                .map(|(_, field)| field.port.clone())
        })
        .collect()
}

/// Computes an island's static wiring from its contracts and the run
/// signatures of its instantiated members, recording Value port types.
fn plan_island(
    compiled: &CompiledGraph,
    wiring: &Wiring<'_>,
    index: usize,
    members: &[NodeId],
    signatures: &[RunSignature],
    input_types: &mut HashMap<PortRef, Type>,
    output_types: &mut HashMap<PortRef, Type>,
) -> Result<IslandPlan, RuntimeError> {
    let position: HashMap<&NodeId, usize> =
        members.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let mut plans = Vec::with_capacity(members.len());
    for (i, node) in members.iter().enumerate() {
        let contract = contract_of(compiled, node)?;
        let signature = &signatures[i];
        let internal_producer = |port: &str| {
            let conn = wiring
                .writer
                .get(&PortRef::new(node.clone(), port.to_string()))?;
            position.get(&conn.from.node).copied()
        };

        let mut deps = BTreeSet::new();
        let inputs = signature.inputs.as_ref().map(|fields| {
            fields
                .iter()
                .filter_map(|(name, ty)| {
                    let port = contract.inputs.iter().find(|p| p.name.as_str() == name)?;
                    let producer = internal_producer(name);
                    if let Some(p) = producer {
                        deps.insert(p);
                    }
                    if port.kind == PortKind::Value {
                        let payload = match (port.optional, ty) {
                            (true, Type::Option(option)) => option.ty(),
                            _ => ty.clone(),
                        };
                        input_types.insert(PortRef::new(node.clone(), port.name.clone()), payload);
                    }
                    Some(InputField {
                        name: port.name.clone(),
                        port: PortRef::new(node.clone(), port.name.clone()),
                        kind: port.kind,
                        optional: port.optional,
                        source: if producer.is_some() {
                            InputSource::Member
                        } else {
                            InputSource::External
                        },
                    })
                })
                .collect()
        });

        for (name, ty) in &signature.outputs {
            if let Some(port) = contract
                .outputs
                .iter()
                .find(|p| p.name.as_str() == name && p.kind == PortKind::Value)
            {
                output_types.insert(PortRef::new(node.clone(), port.name.clone()), ty.clone());
            }
        }

        let outputs = contract
            .outputs
            .iter()
            .map(|port| {
                let consumers = wiring
                    .readers
                    .get(&PortRef::new(node.clone(), port.name.clone()))
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        let member = *position.get(&c.to.node)?;
                        let field = signatures[member]
                            .inputs
                            .as_ref()?
                            .iter()
                            .position(|(name, _)| c.to.port.as_str() == name)?;
                        Some(Consumer {
                            member,
                            field,
                            unwrap_option: wiring.unwraps.contains(&c.id),
                        })
                    })
                    .collect();
                (
                    port.name.clone(),
                    OutputField {
                        kind: port.kind,
                        consumers,
                    },
                )
            })
            .collect();

        plans.push(MemberPlan {
            node: node.clone(),
            inputs,
            has_result: signature.has_result,
            outputs,
            deps: deps.into_iter().collect(),
            dependents: Vec::new(),
        });
    }
    for i in 0..plans.len() {
        for d in plans[i].deps.clone() {
            plans[d].dependents.push(i);
        }
    }
    Ok(IslandPlan {
        index,
        members: plans,
    })
}

/// The graph's connections, indexed by port.
pub(super) struct Wiring<'a> {
    /// The non-feedback connection writing each input port.
    pub(super) writer: HashMap<&'a PortRef, &'a Connection>,
    /// The non-feedback connections reading each output port.
    readers: HashMap<&'a PortRef, Vec<&'a Connection>>,
    /// Every feedback connection.
    feedback: Vec<&'a Connection>,
    /// The connections that unwrap an option
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    unwraps: HashSet<&'a ConnectionId>,
}

impl<'a> Wiring<'a> {
    fn new(compiled: &'a CompiledGraph) -> Self {
        let mut writer = HashMap::new();
        let mut readers: HashMap<&PortRef, Vec<&Connection>> = HashMap::new();
        let mut feedback = Vec::new();
        let mut unwraps = HashSet::new();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            if resolved.unwraps_option {
                unwraps.insert(&conn.id);
            }
            if conn.feedback {
                feedback.push(conn);
            } else {
                writer.insert(&conn.to, conn);
                readers.entry(&conn.from).or_default().push(conn);
            }
        }
        Self {
            writer,
            readers,
            feedback,
            unwraps,
        }
    }
}

/// An island's place in the graph-wide node index.
pub(super) struct IslandWiring {
    /// Where its members start in the graph-wide node index.
    pub(super) offset: usize,
    /// Per member, the nodes (graph-wide index) writing one of its inputs
    /// over a non-feedback connection.
    pub(super) preds: Vec<Vec<usize>>,
    /// The nodes (graph-wide index) writing one of its inputs over a
    /// feedback connection.
    pub(super) feedback_sources: Vec<usize>,
    /// The feedback connections out of its members.
    pub(super) feedback_out: Vec<ConnectionId>,
}

/// Every island's [`IslandWiring`], in island order.
fn node_wiring(compiled: &CompiledGraph, wiring: &Wiring<'_>) -> Vec<IslandWiring> {
    let mut position: HashMap<&NodeId, (usize, usize, usize)> = HashMap::new();
    let mut wired = Vec::with_capacity(compiled.islands().len());
    let mut next = 0;
    for (island, members) in compiled.islands().iter().enumerate() {
        for (member, node) in members.iter().enumerate() {
            position.insert(node, (island, member, next + member));
        }
        wired.push(IslandWiring {
            offset: next,
            preds: vec![Vec::new(); members.len()],
            feedback_sources: Vec::new(),
            feedback_out: Vec::new(),
        });
        next += members.len();
    }
    let mut seen: HashSet<(usize, usize, usize)> = HashSet::new();
    for conn in wiring.writer.values() {
        if let (Some(&(_, _, from)), Some(&(island, member, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
            && seen.insert((island, member, from))
        {
            wired[island].preds[member].push(from);
        }
    }
    for conn in &wiring.feedback {
        if let (Some(&(source, _, from)), Some(&(island, _, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
        {
            wired[source].feedback_out.push(conn.id.clone());
            if !wired[island].feedback_sources.contains(&from) {
                wired[island].feedback_sources.push(from);
            }
        }
    }
    wired
}
