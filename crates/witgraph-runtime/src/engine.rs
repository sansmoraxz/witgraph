//! Wasmtime plumbing: the engine, island Stores, and the generation driver.
//!
//! # Islands
//!
//! An island ([`CompiledGraph::islands`](witgraph_ir::CompiledGraph::islands))
//! is a set of nodes joined by stream/future connections. All of its nodes
//! are instantiated in one wasmtime [`Store`], because a component-model
//! stream or future handle belongs to one Store and the host cannot move
//! items of an arbitrary payload type between Stores. A node with no
//! stream or future connection is an island of one, unless compilation
//! merged it into a stream island to keep the islands a DAG.
//!
//! # Generations
//!
//! `run_generation` drives one generation of an island inside
//! [`Store::run_concurrent`]. Invariants:
//!
//! - A member's `run` is called once every in-island producer it reads
//!   from (over a non-feedback connection, of any kind) has returned. Its
//!   arguments are those producers' fresh outputs plus the host-supplied
//!   external Value inputs. Stream/future handles are passed straight from
//!   the producer's results into the consumer's arguments; the host never
//!   touches their items.
//! - Calls are concurrent: a member's call starts as soon as its own
//!   dependencies have returned, independently of its siblings.
//! - A stream/future output with no in-island consumer is closed as soon as
//!   its `run` returns, so the guest's writes fail instead of blocking.
//! - Each `run` start and return is reported as an island event; a
//!   return carries the member's Value outputs, so the host can latch them
//!   and wake downstream islands while this generation is still going
//!   (once no other member upstream of them is still to return).
//! - The generation finishes when every `run` has returned **and** no guest
//!   task is left in the Store
//!   ([`Accessor::poll_no_interesting_tasks`]): work a guest spawned after
//!   returning, such as a stream writer, belongs to the generation.
//! - Any error, a trap in a spawned task included, faults the whole island:
//!   a trap poisons its Store.
//!
//! # Embedding
//!
//! An island Store's data is the embedder's [`Host::Data`], which carries
//! witgraph's own [`HostState`] ([`IslandData`]). The [`Host`] links each
//! node's capability imports and creates the data of every island Store,
//! on load and on every rebuild, so capability state can live per island.
//!
//! # Sandboxing
//!
//! Every island Store meters fuel. `fuel_async_yield_interval` makes a busy
//! island yield to the executor every `yield_interval` units, so other
//! islands keep running and a timeout around a tick can fire; there is no
//! interleaving *within* one Store. The Store's fuel is reset to the
//! per-run budget before every `run` call (the budget is shared by
//! everything executing in the island, spawned tasks included). An island
//! that burns the budget before its next `run` starts traps with
//! [`NodeFault::FuelExhausted`]; that includes an endless streaming
//! generation once it has consumed the budget, so endless islands need an
//! unlimited (`None`) or suitably large budget.
//!
//! Each Store also caps the linear memory and tables its instances may hold
//! in total (`max_island_memory`), how many instances, memories and tables
//! it may hold (in proportion to its members), and the host memory the
//! values lifted out of a guest in a single call may take (wasmtime's
//! hostcall fuel). Exceeding any of them faults the island.

use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use futures::stream::{FuturesUnordered, StreamExt};
use wasmtime::component::{Accessor, Func, InstancePre, Linker, Type, Val};
use wasmtime::{AsContextMut, Engine, Store, StoreContextMut};
use witgraph_ir::wasm_wave::ast::{Node, NodeType};
use witgraph_ir::wasm_wave::untyped::UntypedValue;
use witgraph_ir::wasm_wave::wasm::{WasmType, WasmValue};
use witgraph_ir::{ComponentContract, NodeId, PortKind, PortName, PortRef};

use crate::error::NodeFault;

/// witgraph's own state in every island Store: who called `fatal`, and the
/// island's memory accounting.
///
/// It has no public API and no public constructor: witgraph creates one per
/// island Store and hands it to [`Host::island_data`], whose data must keep
/// exactly that value and return it from [`IslandData::host_state`] for the
/// Store's whole life. It carries the island's memory limit, so replacing it
/// is not possible from outside the crate.
#[derive(Debug)]
pub struct HostState {
    /// The first `fatal` call in this Store: the calling node and message.
    fatal: Option<(NodeId, String)>,
    /// Bytes the island's linear memories and tables have been granted, in
    /// total.
    memory: usize,
    /// The most `memory` may grow to; `None` is unlimited.
    memory_limit: Option<usize>,
    /// How many members the island has: what its item limits scale with.
    members: usize,
}

/// Bytes charged for one table element: a reference.
const TABLE_ELEMENT_BYTES: usize = 8;

/// The most core instances one island Store may hold, per member. A
/// component built by wit-component has a handful.
pub(crate) const INSTANCES_PER_MEMBER: usize = 64;

/// The most linear memories, and the most tables, one island Store may
/// hold, per member. Every memory reserves address space up front
/// (`memory_reservation`) whatever its size, so their number is capped
/// apart from `max_island_memory`: a crafted component cannot exhaust the
/// process's address space with empty memories.
pub(crate) const MEMORIES_PER_MEMBER: usize = 16;

/// Growing a linear memory or table would take an island past its
/// `max_island_memory`.
#[derive(Debug)]
pub(crate) struct MemoryLimitExceeded {
    pub(crate) requested: usize,
    pub(crate) limit: usize,
}

impl std::fmt::Display for MemoryLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the island needs {} bytes of memory, over its limit of {}",
            self.requested, self.limit
        )
    }
}

impl std::error::Error for MemoryLimitExceeded {}

impl HostState {
    /// Fresh state for an island of `members` nodes limited to
    /// `memory_limit` bytes.
    pub(crate) fn new(memory_limit: Option<usize>, members: usize) -> Self {
        Self {
            fatal: None,
            memory: 0,
            memory_limit,
            members: members.max(1),
        }
    }

    /// Charges a growth of `bytes` against the island's limit.
    fn grow(&mut self, bytes: usize) -> wasmtime::Result<bool> {
        let requested = self.memory.saturating_add(bytes);
        if let Some(limit) = self.memory_limit
            && requested > limit
        {
            return Err(wasmtime::Error::new(MemoryLimitExceeded {
                requested,
                limit,
            }));
        }
        self.memory = requested;
        Ok(true)
    }
}

/// Linear memory and tables both count against `max_island_memory`, as
/// granted. A growth past a memory's or table's own declared maximum is
/// refused without being charged. Nothing is ever refunded: wasmtime
/// reports growth failures it never asked about, so a refund could be
/// forged; a granted growth that wasmtime fails after all (the host is out
/// of memory) stays charged.
impl wasmtime::ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        self.grow(desired.saturating_sub(current))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        let elements = desired.saturating_sub(current);
        self.grow(elements.saturating_mul(TABLE_ELEMENT_BYTES))
    }

    fn instances(&self) -> usize {
        self.members.saturating_mul(INSTANCES_PER_MEMBER)
    }

    fn tables(&self) -> usize {
        self.members.saturating_mul(MEMORIES_PER_MEMBER)
    }

    fn memories(&self) -> usize {
        self.members.saturating_mul(MEMORIES_PER_MEMBER)
    }
}

/// The data of an island Store: the embedder's own state for its
/// capabilities, plus witgraph's [`HostState`].
pub trait IslandData: Send + 'static {
    /// witgraph's state in this Store.
    fn host_state(&mut self) -> &mut HostState;
}

impl IslandData for HostState {
    fn host_state(&mut self) -> &mut HostState {
        self
    }
}

/// What an embedder provides to a runtime graph: each node's capability
/// imports, and the data of each island Store.
///
/// The Store data type is the embedder's, so capability state can live in
/// it (per island, rebuilt with the island) and implementations that need
/// a particular data type (WASI, `bindgen!` hosts) can be linked.
pub trait Host: 'static {
    /// The data of every island Store.
    type Data: IslandData;

    /// Adds `node`'s capability imports to its linker. Called once per
    /// node, at load, after the built-in `witgraph:runtime/host` interface
    /// has been added; the linker is kept for every rebuild of the node's
    /// island. `contract` lists what the node's world imports
    /// ([`ComponentContract::capabilities`]).
    fn link(
        &self,
        node: &NodeId,
        contract: &ComponentContract,
        linker: &mut Linker<Self::Data>,
    ) -> wasmtime::Result<()>;

    /// The data of a new Store for the island holding `members` (in island
    /// order), wrapping `state`: [`IslandData::host_state`] must return
    /// that very value for the Store's life (it carries the island's memory
    /// limit). Called when the island is built at load, and again every
    /// time it is rebuilt after a fault, cancel, shutdown or restore. An
    /// error fails the load ([`RuntimeError::Instantiation`]) or the
    /// rebuild ([`NodeFault::Restart`]).
    ///
    /// [`RuntimeError::Instantiation`]: crate::RuntimeError::Instantiation
    fn island_data(&self, members: &[NodeId], state: HostState) -> wasmtime::Result<Self::Data>;
}

/// The host of a graph whose components import no capabilities.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCapabilities;

impl Host for NoCapabilities {
    type Data = HostState;

    fn link(
        &self,
        _: &NodeId,
        _: &ComponentContract,
        _: &mut Linker<HostState>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }

    fn island_data(&self, _: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }
}

/// Creates the engine every island of one graph shares.
///
/// Besides the async ABI, it enables every component-model value type that
/// lowering lets through in a capability's signature (`map`,
/// `error-context`, fixed-length lists), so a world that lowers also loads.
/// Port payloads are narrower: lowering rejects those types there.
///
/// `memory_reservation` overrides how much address space each linear
/// memory reserves up front, rounded up to whole 64 KiB pages (wasmtime's
/// default is 4 GiB plus guards, which lets compiled code skip bounds
/// checks but caps a 64-bit process at about 32k memories); a memory that
/// outgrows its reservation moves instead.
pub(crate) fn new_engine(memory_reservation: Option<u64>) -> wasmtime::Result<Engine> {
    const PAGE: u64 = 64 * 1024;
    let mut config = wasmtime::Config::new();
    if let Some(bytes) = memory_reservation {
        let rounded = bytes.checked_next_multiple_of(PAGE).ok_or_else(|| {
            wasmtime::format_err!("memory_reservation {bytes} is too large to round to whole pages")
        })?;
        config.memory_reservation(rounded);
    }
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .wasm_component_model_map(true)
        .wasm_component_model_error_context(true)
        .wasm_component_model_fixed_length_lists(true)
        .concurrency_support(true)
        .consume_fuel(true);
    Engine::new(&config)
}

/// The fully qualified name of the built-in host interface.
const HOST_INTERFACE: &str = "witgraph:runtime/host@0.1.0";

/// A linker for one node: the built-in `witgraph:runtime/host` (whose
/// `fatal` knows which node called it) plus the host's capabilities.
pub(crate) fn node_linker<H: Host>(
    engine: &Engine,
    node: &NodeId,
    contract: &ComponentContract,
    host: &H,
) -> wasmtime::Result<Linker<H::Data>> {
    let mut linker = Linker::new(engine);
    let caller = node.clone();
    linker.instance(HOST_INTERFACE)?.func_wrap(
        "fatal",
        move |mut store: StoreContextMut<'_, H::Data>, (message,): (String,)| {
            let state = store.data_mut().host_state();
            if state.fatal.is_none() {
                state.fatal = Some((caller.clone(), message.clone()));
            }
            Err::<(), _>(wasmtime::format_err!("fatal: {message}"))
        },
    )?;
    host.link(node, contract, &mut linker)?;
    Ok(linker)
}

/// Settings applied to every island Store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoreSettings {
    /// Fuel between cooperative yields to the executor.
    pub(crate) yield_interval: u64,
    /// Fuel an island may burn between two `run` starts; `None` is
    /// effectively unlimited.
    pub(crate) fuel_per_run: Option<u64>,
    /// Wasmtime's hostcall fuel: how much host memory the values one call
    /// copies between guest and host may take.
    pub(crate) hostcall_fuel: usize,
}

/// Fuel that never runs out in practice.
const UNLIMITED_FUEL: u64 = u64::MAX / 2;

impl StoreSettings {
    /// The fuel level set before every `run` call.
    pub(crate) fn budget(self) -> u64 {
        self.fuel_per_run.unwrap_or(UNLIMITED_FUEL)
    }
}

/// Where an input field of a member's `inputs` record comes from in a
/// generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSource {
    /// Supplied by the host from its latched input values: unconnected
    /// ports, feedback connections, and connections from other islands.
    External,
    /// Produced by another member of the island earlier in the same
    /// generation (routed through [`OutputField::consumers`]).
    Member,
}

/// One field of a member's `inputs` record, in declaration order.
#[derive(Debug, Clone)]
pub(crate) struct InputField {
    pub(crate) name: PortName,
    /// The member's port this field is.
    pub(crate) port: PortRef,
    pub(crate) kind: PortKind,
    /// Declared `option<T>`: the host wraps the value (or passes `none`).
    pub(crate) optional: bool,
    pub(crate) source: InputSource,
}

/// An in-island reader of an output, over a non-feedback connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Consumer {
    /// The reading member.
    pub(crate) member: usize,
    /// The field of its `inputs` record the connection writes.
    pub(crate) field: usize,
    /// Whether the connection unwraps an option
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    pub(crate) unwrap_option: bool,
}

/// One field of a member's `outputs` record.
#[derive(Debug, Clone)]
pub(crate) struct OutputField {
    pub(crate) kind: PortKind,
    /// Its in-island consumers. At most one for a stream or future.
    pub(crate) consumers: Vec<Consumer>,
}

/// Static wiring of one island member, computed at load.
#[derive(Debug, Clone)]
pub(crate) struct MemberPlan {
    pub(crate) node: NodeId,
    /// `None` when `run` takes no `inputs` record.
    pub(crate) inputs: Option<Vec<InputField>>,
    /// Whether `run` returns an `outputs` record.
    pub(crate) has_result: bool,
    pub(crate) outputs: HashMap<PortName, OutputField>,
    /// Members whose `run` must return before this one is called.
    pub(crate) deps: Vec<usize>,
    /// Members that list this one in their `deps`.
    pub(crate) dependents: Vec<usize>,
}

impl MemberPlan {
    /// The member's Value inputs the host supplies (unconnected ones, and
    /// ones fed by feedback connections or from other islands), with their
    /// field index.
    pub(crate) fn external_values(&self) -> impl Iterator<Item = (usize, &InputField)> {
        self.inputs
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, f)| f.source == InputSource::External && f.kind == PortKind::Value)
    }

    /// How many fields the member's `inputs` record has.
    pub(crate) fn field_count(&self) -> usize {
        self.inputs.as_ref().map_or(0, Vec::len)
    }
}

/// Static wiring of one island, computed at load. Members are in
/// topological order.
#[derive(Debug, Clone)]
pub(crate) struct IslandPlan {
    pub(crate) index: usize,
    pub(crate) members: Vec<MemberPlan>,
}

/// What instantiating one node needs: its component, pre-linked once
/// against its linker (the built-in host interface plus the embedder's
/// capabilities), and the name its `node` interface is exported under.
pub(crate) struct NodeBinary<D: 'static> {
    pub(crate) pre: InstancePre<D>,
    /// `node` for an inline interface, the interface's full id for a named
    /// one (see `witgraph_wit::lower::Lowered::export`).
    pub(crate) export: String,
}

/// A live island's wasmtime side: its Store and each member's `run` export.
/// The island's scheduling state is in [`crate::island`].
pub(crate) struct IslandStore<D: 'static> {
    store: Store<D>,
    runs: Vec<Func>,
}

/// The shape of a member's `run` export as the component declares it.
pub(crate) struct RunSignature {
    /// The `inputs` record's fields in declaration order, with their
    /// component types; `None` when `run` takes no parameter.
    pub(crate) inputs: Option<Vec<(String, Type)>>,
    /// Whether `run` returns a result.
    pub(crate) has_result: bool,
    /// The `outputs` record's fields with their component types; empty
    /// when `run` returns nothing.
    pub(crate) outputs: Vec<(String, Type)>,
}

/// Why an island could not be built.
#[derive(Debug)]
pub(crate) struct BuildError {
    /// The member that failed.
    pub(crate) node: NodeId,
    pub(crate) message: String,
    /// The message of a `fatal` call made while instantiating (from a start
    /// function).
    pub(crate) fatal: Option<String>,
}

/// Instantiates every member of an island into a fresh Store.
///
/// `members` pairs each member's node with what instantiating it needs, in
/// the island plan's order.
pub(crate) async fn build_island<D: IslandData>(
    engine: &Engine,
    members: &[(&NodeId, &NodeBinary<D>)],
    data: D,
    settings: StoreSettings,
) -> Result<(IslandStore<D>, Vec<RunSignature>), BuildError> {
    let mut store = Store::new(engine, data);
    store.limiter(|data: &mut D| data.host_state() as &mut dyn wasmtime::ResourceLimiter);
    // Instantiation (a guest's `_initialize`, say) gets a budget of its
    // own, as big as a `run`'s: a spinning start function faults instead
    // of hanging the load. Every `run` resets the fuel again.
    let setup = |store: &mut Store<D>| -> wasmtime::Result<()> {
        store.fuel_async_yield_interval(Some(settings.yield_interval))?;
        store.set_fuel(settings.budget())?;
        store.set_hostcall_fuel(settings.hostcall_fuel);
        Ok(())
    };
    let first = members
        .first()
        .map_or_else(|| NodeId::from("?"), |(node, _)| (*node).clone());
    let plain = |node: &NodeId, message: String| BuildError {
        node: node.clone(),
        message,
        fatal: None,
    };
    setup(&mut store).map_err(|e| plain(&first, format!("{e:#}")))?;

    let mut runs = Vec::with_capacity(members.len());
    let mut signatures = Vec::with_capacity(members.len());
    for (node, binary) in members {
        let instance = match binary.pre.instantiate_async(&mut store).await {
            Ok(instance) => instance,
            Err(e) => {
                let fatal = store.data_mut().host_state().fatal.take();
                return Err(BuildError {
                    node: (*node).clone(),
                    message: format!("{e:#}"),
                    fatal: fatal.map(|(_, message)| message),
                });
            }
        };
        let missing = |what: &str| plain(node, format!("component exports no `{what}`"));
        let export = binary.export.as_str();
        let node_export = instance
            .get_export_index(&mut store, None, export)
            .ok_or_else(|| missing(export))?;
        let run_export = instance
            .get_export_index(&mut store, Some(&node_export), "run")
            .ok_or_else(|| missing(&format!("{export}#run")))?;
        let run = instance
            .get_func(&mut store, run_export)
            .ok_or_else(|| missing(&format!("{export}#run function")))?;
        signatures.push(signature(&store, run));
        runs.push(run);
    }
    store
        .set_fuel(settings.budget())
        .map_err(|e| plain(&first, format!("{e:#}")))?;
    Ok((IslandStore { store, runs }, signatures))
}

fn signature<D>(store: &Store<D>, run: Func) -> RunSignature {
    let ty = run.ty(store);
    let inputs = ty.params().next().map(|(_, param)| match param {
        Type::Record(record) => record
            .fields()
            .map(|field| (field.name.to_string(), field.ty))
            .collect(),
        _ => Vec::new(),
    });
    let outputs = match ty.results().next() {
        Some(Type::Record(record)) => record
            .fields()
            .map(|field| (field.name.to_string(), field.ty))
            .collect(),
        _ => Vec::new(),
    };
    RunSignature {
        inputs,
        has_result: ty.results().len() > 0,
        outputs,
    }
}

/// A report from an in-flight generation.
#[derive(Debug)]
pub(crate) struct IslandEvent {
    pub(crate) island: usize,
    pub(crate) generation: u64,
    pub(crate) kind: IslandEventKind,
}

/// What happened in a generation, to the member at `member` in the island
/// plan's order.
#[derive(Debug)]
pub(crate) enum IslandEventKind {
    RunStarted {
        member: usize,
    },
    RunReturned {
        member: usize,
        /// The member's Value outputs, by port.
        values: Vec<(PortName, Val)>,
    },
    /// The stopped island was rebuilt into a fresh Store; its generation
    /// starts now.
    Rebuilt,
}

/// How a generation ended: the island back, or the fault that killed it
/// (with the node that called `fatal`, if one did).
pub(crate) type Outcome<D> = Result<IslandStore<D>, (NodeFault, Option<NodeId>)>;

/// One generation to run.
pub(crate) struct Generation {
    pub(crate) plan: Arc<IslandPlan>,
    /// Per member, per field of its `inputs` record, the host-supplied
    /// Value input (`None` for an absent one, and for fields members of
    /// the island write).
    pub(crate) external: Vec<Vec<Option<Val>>>,
    pub(crate) settings: StoreSettings,
    /// The island's generation number.
    pub(crate) number: u64,
    pub(crate) events: UnboundedSender<IslandEvent>,
}

/// Runs one generation of `island`.
pub(crate) async fn run_generation<D: IslandData>(
    mut island: IslandStore<D>,
    generation: Generation,
) -> Outcome<D> {
    let Generation {
        plan,
        external,
        settings,
        number: generation,
        events,
    } = generation;
    island.store.data_mut().host_state().fatal = None;
    let budget = settings.budget();
    let runs = island.runs.clone();
    let send = |kind| {
        // The receiver lives as long as the runtime graph; a send can only
        // fail while the graph is being dropped.
        let _ = events.unbounded_send(IslandEvent {
            island: plan.index,
            generation,
            kind,
        });
    };
    let driven = island
        .store
        .run_concurrent(async |acc| -> wasmtime::Result<()> {
            drive(acc, &plan, &runs, external, budget, &send).await?;
            poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
            Ok(())
        })
        .await;
    match driven.and_then(|inner| inner) {
        Ok(()) => Ok(island),
        Err(error) => {
            let fatal = island.store.data_mut().host_state().fatal.take();
            Err(classify(&error, fatal))
        }
    }
}

/// Rebuilds a stopped island into a fresh Store, then runs one generation
/// in it. Both are the generation: the rebuild runs inside the generation's
/// future, beside every other island, and survives a dropped tick. A failed
/// rebuild is the fault [`NodeFault::Restart`], or [`NodeFault::Fatal`]
/// when a member's start function called `fatal`.
pub(crate) async fn rebuild_and_run<D: IslandData>(
    engine: Engine,
    members: Vec<(NodeId, Arc<NodeBinary<D>>)>,
    data: wasmtime::Result<D>,
    generation: Generation,
) -> Outcome<D> {
    let parts: Vec<(&NodeId, &NodeBinary<D>)> = members
        .iter()
        .map(|(node, binary)| (node, &**binary))
        .collect();
    let data = match data {
        Ok(data) => data,
        Err(e) => {
            let message = format!("island data: {e:#}");
            return Err((NodeFault::Restart { message }, None));
        }
    };
    let island = match build_island(&engine, &parts, data, generation.settings).await {
        Ok((island, _)) => island,
        Err(BuildError {
            node,
            fatal: Some(message),
            ..
        }) => return Err((NodeFault::Fatal { message }, Some(node))),
        Err(BuildError { node, message, .. }) => {
            let message = format!("`{node}`: {message}");
            return Err((NodeFault::Restart { message }, Some(node)));
        }
    };
    let _ = generation.events.unbounded_send(IslandEvent {
        island: generation.plan.index,
        generation: generation.number,
        kind: IslandEventKind::Rebuilt,
    });
    run_generation(island, generation).await
}

/// Calls every member's `run` in dependency order, concurrently, and
/// routes outputs. Returns once every call has returned.
async fn drive<D: IslandData>(
    acc: &Accessor<D>,
    plan: &IslandPlan,
    runs: &[Func],
    mut args: Vec<Vec<Option<Val>>>,
    budget: u64,
    send: &impl Fn(IslandEventKind),
) -> wasmtime::Result<()> {
    let mut waiting: Vec<usize> = plan.members.iter().map(|m| m.deps.len()).collect();
    let mut calls = FuturesUnordered::new();
    for (i, member) in plan.members.iter().enumerate() {
        if waiting[i] == 0 {
            let params = params(member, std::mem::take(&mut args[i]))?;
            send(IslandEventKind::RunStarted { member: i });
            calls.push(call(acc, runs[i], params, member.has_result, budget, i));
        }
    }

    while let Some((i, results)) = calls.next().await {
        let results = results?;
        let member = &plan.members[i];
        let mut values = Vec::new();
        if let Some(Val::Record(fields)) = results.into_iter().next() {
            for (name, val) in fields {
                let port = PortName::from(name);
                let Some(output) = member.outputs.get(&port) else {
                    continue;
                };
                match output.kind {
                    PortKind::Value => {
                        for consumer in &output.consumers {
                            let delivered = if consumer.unwrap_option {
                                option_payload(&val).cloned()
                            } else {
                                Some(val.clone())
                            };
                            args[consumer.member][consumer.field] = delivered;
                        }
                        values.push((port, val));
                    }
                    PortKind::Stream | PortKind::Future => match output.consumers.first() {
                        Some(consumer) => args[consumer.member][consumer.field] = Some(val),
                        None => close(acc, val)?,
                    },
                }
            }
        }
        send(IslandEventKind::RunReturned { member: i, values });
        for &k in &member.dependents {
            waiting[k] -= 1;
            if waiting[k] == 0 {
                let target = &plan.members[k];
                let params = params(target, std::mem::take(&mut args[k]))?;
                send(IslandEventKind::RunStarted { member: k });
                calls.push(call(acc, runs[k], params, target.has_result, budget, k));
            }
        }
    }
    Ok(())
}

async fn call<D: IslandData>(
    acc: &Accessor<D>,
    run: Func,
    params: Vec<Val>,
    has_result: bool,
    budget: u64,
    index: usize,
) -> (usize, wasmtime::Result<Vec<Val>>) {
    if let Err(e) = acc.with(|mut access| access.as_context_mut().set_fuel(budget)) {
        return (index, Err(e));
    }
    let mut results = if has_result {
        vec![Val::Bool(false)]
    } else {
        Vec::new()
    };
    let outcome = run.call_concurrent(acc, &params, &mut results).await;
    (index, outcome.map(|()| results))
}

/// Builds `run`'s parameter list from a member's collected inputs, one per
/// field of its `inputs` record.
fn params(member: &MemberPlan, args: Vec<Option<Val>>) -> wasmtime::Result<Vec<Val>> {
    let Some(fields) = &member.inputs else {
        return Ok(Vec::new());
    };
    let mut record = Vec::with_capacity(fields.len());
    let mut args = args.into_iter();
    for field in fields {
        let value = args.next().flatten();
        let value = if field.optional {
            Val::Option(value.map(Box::new))
        } else {
            value.ok_or_else(|| {
                wasmtime::format_err!(
                    "no value for required input `{}.{}`",
                    member.node,
                    field.name
                )
            })?
        };
        record.push((field.name.to_string(), value));
    }
    Ok(vec![Val::Record(record)])
}

/// Drops the host's copy of an unconsumed stream or future handle so the
/// guest's writes fail instead of blocking.
fn close<D: IslandData>(acc: &Accessor<D>, val: Val) -> wasmtime::Result<()> {
    match val {
        Val::Stream(mut stream) => acc.with(|mut access| stream.close(&mut access)),
        Val::Future(mut future) => acc.with(|mut access| future.close(&mut access)),
        _ => Ok(()),
    }
}

fn classify(
    error: &wasmtime::Error,
    fatal: Option<(NodeId, String)>,
) -> (NodeFault, Option<NodeId>) {
    if let Some((node, message)) = fatal {
        return (NodeFault::Fatal { message }, Some(node));
    }
    if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
        return (NodeFault::FuelExhausted, None);
    }
    if let Some(exceeded) = error.downcast_ref::<MemoryLimitExceeded>() {
        let MemoryLimitExceeded { requested, limit } = *exceeded;
        return (NodeFault::MemoryLimit { requested, limit }, None);
    }
    // Wasmtime's error type for this is private: it is told apart by name.
    if error
        .chain()
        .any(|cause| format!("{cause:?}") == "HostcallFuelExhausted")
    {
        return (NodeFault::HostcallFuelExhausted, None);
    }
    (
        NodeFault::WasmTrap {
            message: format!("{error:#}"),
        },
        None,
    )
}

/// What a connection that unwraps an option delivers for `val`: the
/// payload of `some`, or `None` for `none` (an absent optional input).
pub(crate) fn option_payload(val: &Val) -> Option<&Val> {
    match val {
        Val::Option(payload) => payload.as_deref(),
        other => Some(other),
    }
}

/// Parses WAVE text as a value of type `ty`, in [`canonical`] form. Unlike
/// WAVE's own parser, it rejects record fields, cases and flags the type
/// does not have, and fields given twice.
pub(crate) fn parse_wave(ty: &Type, text: &str) -> Result<Val, String> {
    let parsed = UntypedValue::parse(text).map_err(|e| format!("{e:#}"))?;
    check_labels(ty, parsed.node(), text)?;
    let val: Val = parsed.to_wasm_value(ty).map_err(|e| format!("{e:#}"))?;
    Ok(canonical(ty, val))
}

/// Checks every label in parsed WAVE text against `ty`. Shape mismatches
/// are left to the typed conversion, which reports them.
fn check_labels(ty: &Type, node: &Node, src: &str) -> Result<(), String> {
    match (ty, node.ty()) {
        (Type::Record(record), NodeType::Record) => {
            let fields = node.as_record(src).map_err(|e| format!("{e:#}"))?;
            let mut seen = Vec::new();
            for (label, value) in fields {
                if seen.contains(&label) {
                    return Err(format!("field `{label}` is given twice"));
                }
                seen.push(label);
                let field = record
                    .fields()
                    .find(|f| f.name == label)
                    .ok_or_else(|| format!("the record has no field `{label}`"))?;
                check_labels(&field.ty, value, src).map_err(|e| format!("field `{label}`: {e}"))?;
            }
            Ok(())
        }
        (Type::List(list), NodeType::List) => {
            let elem = list.ty();
            let items = node.as_list().map_err(|e| format!("{e:#}"))?;
            items
                .into_iter()
                .try_for_each(|item| check_labels(&elem, item, src))
        }
        (Type::FixedLengthList(list), NodeType::List) => {
            let elem = list.ty();
            let items = node.as_list().map_err(|e| format!("{e:#}"))?;
            items
                .into_iter()
                .try_for_each(|item| check_labels(&elem, item, src))
        }
        (Type::Tuple(tuple), NodeType::Tuple) => {
            let items = node.as_tuple().map_err(|e| format!("{e:#}"))?;
            tuple
                .types()
                .zip(items)
                .try_for_each(|(ty, item)| check_labels(&ty, item, src))
        }
        (Type::Option(option), NodeType::OptionSome | NodeType::OptionNone) => {
            match node.as_option().map_err(|e| format!("{e:#}"))? {
                Some(payload) => check_labels(&option.ty(), payload, src),
                None => Ok(()),
            }
        }
        // An option's payload may be written bare.
        (Type::Option(option), _) => check_labels(&option.ty(), node, src),
        (Type::Result(result), NodeType::ResultOk | NodeType::ResultErr) => {
            match node.as_result().map_err(|e| format!("{e:#}"))? {
                Ok(Some(payload)) => match result.ok() {
                    Some(ty) => check_labels(&ty, payload, src),
                    None => Ok(()),
                },
                Err(Some(payload)) => match result.err() {
                    Some(ty) => check_labels(&ty, payload, src),
                    None => Ok(()),
                },
                _ => Ok(()),
            }
        }
        (Type::Variant(variant), NodeType::Label | NodeType::VariantWithPayload) => {
            let (label, payload) = node.as_variant(src).map_err(|e| format!("{e:#}"))?;
            let case = variant
                .cases()
                .find(|c| c.name == label)
                .ok_or_else(|| format!("the variant has no case `{label}`"))?;
            match (case.ty, payload) {
                (Some(ty), Some(payload)) => check_labels(&ty, payload, src),
                _ => Ok(()),
            }
        }
        (Type::Enum(cases), NodeType::Label) => {
            let label = node.as_enum(src).map_err(|e| format!("{e:#}"))?;
            if cases.names().any(|case| case == label) {
                Ok(())
            } else {
                Err(format!("the enum has no case `{label}`"))
            }
        }
        (Type::Flags(flags), NodeType::Flags) => {
            let set = node.as_flags(src).map_err(|e| format!("{e:#}"))?;
            let mut seen = Vec::new();
            for flag in set {
                if !flags.names().any(|name| name == flag) {
                    return Err(format!("the flags have no flag `{flag}`"));
                }
                if seen.contains(&flag) {
                    return Err(format!("flag `{flag}` is given twice"));
                }
                seen.push(flag);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Checks that `val` structurally has value type `ty` (record field names
/// and order, case names, numeric widths). Handles and resources are never
/// port values.
pub(crate) fn check_type(ty: &Type, val: &Val) -> Result<(), String> {
    let mismatch = || Err(format!("expected {}, found {}", ty.kind(), val.kind()));
    let payload = |ty: Option<Type>, val: &Option<Box<Val>>| match (ty, val) {
        (None, None) => Ok(()),
        (Some(ty), Some(val)) => check_type(&ty, val),
        (Some(_), None) => Err("missing payload".to_string()),
        (None, Some(_)) => Err("unexpected payload".to_string()),
    };
    match (ty, val) {
        (Type::Bool, Val::Bool(_))
        | (Type::S8, Val::S8(_))
        | (Type::U8, Val::U8(_))
        | (Type::S16, Val::S16(_))
        | (Type::U16, Val::U16(_))
        | (Type::S32, Val::S32(_))
        | (Type::U32, Val::U32(_))
        | (Type::S64, Val::S64(_))
        | (Type::U64, Val::U64(_))
        | (Type::Float32, Val::Float32(_))
        | (Type::Float64, Val::Float64(_))
        | (Type::Char, Val::Char(_))
        | (Type::String, Val::String(_)) => Ok(()),
        (Type::List(list), Val::List(items)) => {
            let elem = list.ty();
            items.iter().try_for_each(|item| check_type(&elem, item))
        }
        (Type::FixedLengthList(list), Val::FixedLengthList(items)) => {
            if items.len() != list.len() as usize {
                return Err(format!(
                    "expected {} elements, found {}",
                    list.len(),
                    items.len()
                ));
            }
            let elem = list.ty();
            items.iter().try_for_each(|item| check_type(&elem, item))
        }
        (Type::Map(map), Val::Map(entries)) => entries.iter().try_for_each(|(k, v)| {
            check_type(&map.key(), k)?;
            check_type(&map.value(), v)
        }),
        (Type::Record(record), Val::Record(fields)) => {
            if record.fields().len() != fields.len() {
                return mismatch();
            }
            record
                .fields()
                .zip(fields)
                .try_for_each(|(field, (name, value))| {
                    if field.name != name {
                        return Err(format!("expected field `{}`, found `{name}`", field.name));
                    }
                    check_type(&field.ty, value).map_err(|e| format!("field `{name}`: {e}"))
                })
        }
        (Type::Tuple(tuple), Val::Tuple(items)) => {
            if tuple.types().len() != items.len() {
                return mismatch();
            }
            tuple
                .types()
                .zip(items)
                .try_for_each(|(ty, item)| check_type(&ty, item))
        }
        (Type::Variant(variant), Val::Variant(name, value)) => {
            match variant.cases().find(|case| case.name == name) {
                Some(case) => payload(case.ty, value).map_err(|e| format!("case `{name}`: {e}")),
                None => Err(format!("no case `{name}`")),
            }
        }
        (Type::Enum(cases), Val::Enum(name)) => {
            if cases.names().any(|case| case == name) {
                Ok(())
            } else {
                Err(format!("no case `{name}`"))
            }
        }
        (Type::Option(option), Val::Option(value)) => match value {
            Some(value) => check_type(&option.ty(), value),
            None => Ok(()),
        },
        (Type::Result(result), Val::Result(value)) => match value {
            Ok(ok) => payload(result.ok(), ok),
            Err(err) => payload(result.err(), err),
        },
        (Type::Flags(flags), Val::Flags(set)) => {
            if let Some(flag) = set
                .iter()
                .find(|flag| !flags.names().any(|name| name == flag.as_str()))
            {
                return Err(format!("no flag `{flag}`"));
            }
            match set
                .iter()
                .enumerate()
                .find(|(i, flag)| set[..*i].contains(flag))
            {
                Some((_, flag)) => Err(format!("flag `{flag}` is set twice")),
                None => Ok(()),
            }
        }
        _ => mismatch(),
    }
}

/// `val` (of type `ty`, as [`check_type`] accepts it) in the one form a
/// guest produces: every flags set in declaration order. Value equality
/// decides whether an input changed, so equal values must compare equal.
pub(crate) fn canonical(ty: &Type, val: Val) -> Val {
    if !has_flags(ty) {
        return val;
    }
    canonical_in(ty, val)
}

/// Whether a `flags` type appears anywhere in `ty`.
fn has_flags(ty: &Type) -> bool {
    let payload = |ty: Option<Type>| ty.is_some_and(|ty| has_flags(&ty));
    match ty {
        Type::Flags(_) => true,
        Type::List(list) => has_flags(&list.ty()),
        Type::FixedLengthList(list) => has_flags(&list.ty()),
        Type::Record(record) => record.fields().any(|f| has_flags(&f.ty)),
        Type::Tuple(tuple) => tuple.types().any(|t| has_flags(&t)),
        Type::Variant(variant) => variant.cases().any(|c| payload(c.ty)),
        Type::Option(option) => has_flags(&option.ty()),
        Type::Result(result) => payload(result.ok()) || payload(result.err()),
        _ => false,
    }
}

/// [`canonical`] without the check for flags: walks the whole value once.
fn canonical_in(ty: &Type, val: Val) -> Val {
    let payload = |ty: Option<Type>, val: Option<Box<Val>>| match (ty, val) {
        (Some(ty), Some(val)) => Some(Box::new(canonical_in(&ty, *val))),
        (_, val) => val,
    };
    match (ty, val) {
        (Type::Flags(flags), Val::Flags(set)) => Val::Flags(
            flags
                .names()
                .filter(|name| set.iter().any(|flag| flag == name))
                .map(String::from)
                .collect(),
        ),
        (Type::List(list), Val::List(items)) => {
            let elem = list.ty();
            Val::List(items.into_iter().map(|v| canonical_in(&elem, v)).collect())
        }
        (Type::FixedLengthList(list), Val::FixedLengthList(items)) => {
            let elem = list.ty();
            Val::FixedLengthList(items.into_iter().map(|v| canonical_in(&elem, v)).collect())
        }
        (Type::Record(record), Val::Record(fields)) => Val::Record(
            record
                .fields()
                .zip(fields)
                .map(|(field, (name, v))| (name, canonical_in(&field.ty, v)))
                .collect(),
        ),
        (Type::Tuple(tuple), Val::Tuple(items)) => Val::Tuple(
            tuple
                .types()
                .zip(items)
                .map(|(ty, v)| canonical_in(&ty, v))
                .collect(),
        ),
        (Type::Variant(variant), Val::Variant(name, value)) => {
            let ty = variant.cases().find(|c| c.name == name).and_then(|c| c.ty);
            Val::Variant(name, payload(ty, value))
        }
        (Type::Option(option), Val::Option(value)) => {
            Val::Option(payload(Some(option.ty()), value))
        }
        (Type::Result(result), Val::Result(value)) => Val::Result(match value {
            Ok(ok) => Ok(payload(result.ok(), ok)),
            Err(err) => Err(payload(result.err(), err)),
        }),
        (_, val) => val,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload type of echo's optional `in` port, read from the
    /// instantiated component.
    async fn echo_in_type() -> Type {
        let engine = new_engine(None).unwrap();
        let node = NodeId::from("e");
        let contract = witgraph_wit::load_components(test_components::ECHO.wit)
            .unwrap()
            .remove(0);
        let component =
            wasmtime::component::Component::new(&engine, test_components::ECHO.wasm).unwrap();
        let linker = node_linker(&engine, &node, &contract, &NoCapabilities).unwrap();
        let binary = NodeBinary {
            pre: linker.instantiate_pre(&component).unwrap(),
            export: "node".into(),
        };
        let settings = StoreSettings {
            yield_interval: 100_000,
            fuel_per_run: None,
            hostcall_fuel: 128 << 20,
        };
        let (_, signatures) = build_island(
            &engine,
            &[(&node, &binary)],
            HostState::new(None, 1),
            settings,
        )
        .await
        .unwrap();
        let fields = signatures[0].inputs.clone().unwrap();
        let (name, ty) = &fields[0];
        assert_eq!(name, "in");
        match ty {
            Type::Option(option) => option.ty(),
            other => panic!("`in` is optional, found {other:?}"),
        }
    }

    #[test]
    fn island_memory_counts_tables_and_skips_failed_growth() {
        use wasmtime::ResourceLimiter;
        let mut state = HostState::new(Some(64 * 1024), 2);
        assert!(state.memory_growing(0, 65536, None).unwrap());
        assert!(
            state.table_growing(0, 16 << 20, None).is_err(),
            "16M table elements are far over 64 KiB"
        );
        // A grow past the memory's own maximum fails, and costs nothing.
        assert!(!state.memory_growing(65536, 131072, Some(65536)).unwrap());
        assert_eq!(state.memory, 65536);
        // A failure wasmtime reports refunds nothing: it could be forged.
        state
            .memory_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        state
            .table_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        assert_eq!(state.memory, 65536);
        let err = state.memory_growing(65536, 131072, None).unwrap_err();
        assert!(err.downcast_ref::<MemoryLimitExceeded>().is_some());
        assert_eq!(state.instances(), 2 * INSTANCES_PER_MEMBER);
        assert_eq!(state.memories(), 2 * MEMORIES_PER_MEMBER);
        assert_eq!(state.tables(), 2 * MEMORIES_PER_MEMBER);
    }

    #[test]
    fn a_reservation_too_large_to_round_is_an_error() {
        assert!(new_engine(Some(u64::MAX)).is_err());
        assert!(new_engine(Some(0)).is_ok());
        assert!(new_engine(Some(1)).is_ok());
    }

    #[test]
    fn option_payloads_unwrap() {
        let some = Val::Option(Some(Box::new(Val::U32(5))));
        assert_eq!(option_payload(&some), Some(&Val::U32(5)));
        assert_eq!(option_payload(&Val::Option(None)), None);
        assert_eq!(option_payload(&Val::U32(5)), Some(&Val::U32(5)));
    }

    /// Change detection is `Val ==`: the README promises how floats compare.
    #[test]
    fn floats_compare_as_documented() {
        assert_ne!(Val::Float64(0.0), Val::Float64(-0.0));
        assert_eq!(Val::Float64(f64::NAN), Val::Float64(-f64::NAN));
        assert_ne!(Val::Float32(0.0), Val::Float32(-0.0));
        assert_eq!(Val::Float32(f32::NAN), Val::Float32(f32::NAN));
    }

    /// The parameter type of a function a hand-written component imports.
    fn param_type(wat: &str) -> Type {
        let engine = new_engine(None).unwrap();
        let bytes = wat::parse_str(wat).unwrap();
        let component = wasmtime::component::Component::new(&engine, bytes).unwrap();
        let ty = component.component_type();
        let (_, item) = ty.imports(&engine).find(|(name, _)| *name == "f").unwrap();
        let wasmtime::component::types::ComponentItem::ComponentFunc(func) = item.ty else {
            panic!("`f` is a function");
        };
        func.params().next().unwrap().1
    }

    #[test]
    fn wave_text_with_labels_the_type_lacks_is_rejected() {
        let ty = param_type(
            r#"(component
                (type $r (record (field "x" u32) (field "y" (option u32))))
                (import "r" (type $r2 (eq $r)))
                (import "f" (func (param "p" $r2))))"#,
        );
        let x_only = Val::Record(vec![
            ("x".into(), Val::U32(1)),
            ("y".into(), Val::Option(None)),
        ]);
        assert_eq!(parse_wave(&ty, "{x: 1}"), Ok(x_only));
        assert!(parse_wave(&ty, "{x: 1, y: some(2)}").is_ok());
        assert!(parse_wave(&ty, "{x: 1, y: 2}").is_ok(), "a bare payload");
        for bad in [
            "{x: 1, yy: some(2)}",
            "{x: 1, y: some(2), z: 7}",
            "{x: 1, x: 2}",
        ] {
            assert!(parse_wave(&ty, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wave_text_with_unknown_cases_and_flags_is_rejected() {
        let ty = param_type(
            r#"(component
                (type $f (flags "a" "b"))
                (import "fl" (type $f2 (eq $f)))
                (type $v (variant (case "on" $f2) (case "off")))
                (import "v" (type $v2 (eq $v)))
                (import "f" (func (param "p" (list $v2)))))"#,
        );
        assert!(parse_wave(&ty, "[on({b, a}), off]").is_ok());
        let canonical = parse_wave(&ty, "[on({b, a})]").unwrap();
        assert_eq!(
            canonical,
            Val::List(vec![Val::Variant(
                "on".into(),
                Some(Box::new(Val::Flags(vec!["a".into(), "b".into()])))
            )])
        );
        for bad in ["[maybe]", "[on({c})]", "[on({a, a})]"] {
            assert!(parse_wave(&ty, bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn wave_text_parses_against_component_types() {
        let ty = echo_in_type().await;
        assert_eq!(parse_wave(&ty, "1.5"), Ok(Val::Float64(1.5)));
        assert!(parse_wave(&ty, "\"wrong\"").is_err());
    }

    #[tokio::test]
    async fn values_are_checked_structurally() {
        let ty = echo_in_type().await;
        assert!(check_type(&ty, &Val::Float64(1.0)).is_ok());
        let err = check_type(&ty, &Val::U32(1)).unwrap_err();
        assert_eq!(err, "expected f64, found u32");
    }
}
