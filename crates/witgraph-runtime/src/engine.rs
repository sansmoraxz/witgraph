//! Wasmtime plumbing: the engine, island Stores, and the generation driver.
//!
//! # Islands
//!
//! An island ([`CompiledGraph::islands`](witgraph_ir::CompiledGraph::islands))
//! is a set of nodes joined by stream/future connections: the unit the
//! scheduler runs, faults and rebuilds. A component-model stream or future
//! handle belongs to one wasmtime [`Store`], so members joined by a handle
//! the host cannot move are instantiated in one Store, and the handle
//! passes straight from one to the other. By default an island is one
//! Store. With [`RuntimeConfig::split_islands`](crate::RuntimeConfig::split_islands),
//! the host moves a stream of scalars or strings itself, by pumping its
//! items between two Stores, and members joined only by such streams each
//! get a Store of their own. A node with no stream or future
//! connection is an island of one, unless compilation merged it into a
//! stream island to keep the islands a DAG.
//!
//! # Generations
//!
//! `run_generation` drives one generation of an island, each of its Stores
//! inside its own [`Store::run_concurrent`], all at once. Invariants:
//!
//! - A member's `run` is called once every in-island producer it reads
//!   from (over a non-feedback connection, of any kind) has returned. Its
//!   arguments are those producers' fresh outputs plus the host-supplied
//!   external Value inputs. Inside a Store, stream/future handles are
//!   passed straight from the producer's results into the consumer's
//!   arguments, and the host never touches their items; between Stores, a
//!   stream is pumped.
//! - Calls are concurrent: a member's call starts as soon as its own
//!   dependencies have returned, independently of its siblings.
//! - A stream/future output with no in-island consumer is closed as soon as
//!   its `run` returns, so the guest's writes fail instead of blocking.
//! - Each `run` start and return is reported as an island event; a
//!   return carries the member's Value outputs, so the host can latch them
//!   and wake downstream islands while this generation is still going
//!   (once no other member upstream of them is still to return).
//! - The generation finishes when every `run` has returned **and** no guest
//!   task is left in any of its Stores
//!   ([`Accessor::poll_no_interesting_tasks`]): work a guest spawned after
//!   returning, such as a stream writer, belongs to the generation. Split,
//!   a Store also runs until every stream it pumps out has ended, whoever
//!   writes it (a node may return a stream it was given), and then cuts
//!   the streams it took in.
//! - Any error, a trap in a spawned task included, faults the whole island:
//!   a trap poisons its Store, and the island's other Stores are dropped
//!   with it.
//!
//! # Embedding
//!
//! A Store's data is the embedder's [`Host::Data`], which carries
//! witgraph's own [`HostState`] ([`IslandData`]). The [`Host`] links each
//! node's capability imports and creates the data of every Store, on load
//! and on every rebuild, so capability state can live per Store.
//!
//! # Sandboxing
//!
//! Every Store meters fuel. `fuel_async_yield_interval` makes a busy Store
//! yield to the executor every `yield_interval` units, so other Stores
//! keep running and a timeout around a tick can fire; there is no
//! interleaving *within* one Store. A Store's fuel is reset to the per-run
//! budget before every `run` call in it (the budget is shared by
//! everything executing in the Store, spawned tasks included). A Store
//! that burns the budget before its next `run` starts traps, and its
//! island faults with [`NodeFault::FuelExhausted`]; that includes an
//! endless streaming generation once it has consumed the budget, so
//! endless islands need an unlimited (`None`) or suitably large budget.
//!
//! An island's Stores share one cap on the linear memory and tables their
//! instances may hold in total (`max_island_memory`). Each Store also caps
//! how many instances, memories and tables it may hold (in proportion to
//! the components its members instantiate, providers included), and the host memory the values lifted out of a guest in a
//! single call may take (wasmtime's hostcall fuel). Exceeding any of them
//! faults the island. Fuel is wasmtime's per Store: in an island of several
//! Stores, each gets the per-run budget.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use futures::FutureExt;
use futures::channel::{mpsc, oneshot};
use futures::stream::{FuturesUnordered, StreamExt};
use wasmtime::component::{Accessor, Func, InstancePre, Linker, Type, Val};
use wasmtime::{AsContextMut, Engine, Store, StoreContextMut};
use witgraph_ir::{
    Capability, ComponentContract, NodeId, PortDirection, PortKind, PortName, PortRef,
};
use witgraph_sched::plan::{IslandPlan, MemberPlan};
use witgraph_sched::{
    Executor, Generation, GenerationFuture, IslandEventKind, NodeCaller, NodeFault, OptionPayload,
    Outcome, drive, missing_input_message,
};

use crate::pump::{Ended, Intake, StreamRx, Tap, export_stream, import_stream};

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
    /// The island's memory accounting, shared by all its Stores.
    memory: Arc<IslandMemory>,
    /// How many components the Store's members instantiate (a member with
    /// links counts its providers too): what its item limits scale with.
    components: usize,
}

/// The memory an island has been granted: its Stores' linear memories and
/// tables, and the stream items the host holds between its Stores
/// ([`crate::pump`]).
#[derive(Debug)]
pub(crate) struct IslandMemory {
    /// Bytes granted so far, in total.
    used: AtomicUsize,
    /// The most `used` may grow to; `None` is unlimited.
    limit: Option<usize>,
}

impl IslandMemory {
    /// Charges `bytes` against the limit.
    pub(crate) fn charge(&self, bytes: usize) -> Result<(), MemoryLimitExceeded> {
        let limit = self.limit;
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let requested = used.saturating_add(bytes);
                limit
                    .is_none_or(|limit| requested <= limit)
                    .then_some(requested)
            })
            .map(|_| ())
            .map_err(|used| MemoryLimitExceeded {
                requested: used.saturating_add(bytes),
                limit: limit.unwrap_or(usize::MAX),
            })
    }

    /// Gives back `bytes` the host charged and no longer holds. Never on a
    /// guest's word (see [`HostState`]'s limiter).
    pub(crate) fn release(&self, bytes: usize) {
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }
}

/// Bytes charged for one table element: a reference.
const TABLE_ELEMENT_BYTES: usize = 8;

/// The most core instances one island Store may hold, per component its
/// members instantiate. A component built by wit-component has a handful.
pub(crate) const INSTANCES_PER_COMPONENT: usize = 64;

/// The most linear memories, and the most tables, one island Store may
/// hold, per component its members instantiate. Every memory reserves address space up front
/// (`memory_reservation`) whatever its size, so their number is capped
/// apart from `max_island_memory`: a crafted component cannot exhaust the
/// process's address space with empty memories.
pub(crate) const MEMORIES_PER_COMPONENT: usize = 16;

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
    /// Fresh state for each Store of an island limited to `memory_limit`
    /// bytes in total, one per entry of `stores`: how many components the
    /// Store's members instantiate.
    pub(crate) fn island(
        memory_limit: Option<usize>,
        stores: impl IntoIterator<Item = usize>,
    ) -> Vec<Self> {
        let memory = Arc::new(IslandMemory {
            used: AtomicUsize::new(0),
            limit: memory_limit,
        });
        stores
            .into_iter()
            .map(|components| Self {
                fatal: None,
                memory: memory.clone(),
                components: components.max(1),
            })
            .collect()
    }

    /// Charges a growth of `bytes` against the island's limit.
    fn grow(&mut self, bytes: usize) -> wasmtime::Result<bool> {
        self.memory.charge(bytes).map_err(wasmtime::Error::new)?;
        Ok(true)
    }

    /// The island's memory accounting.
    pub(crate) fn memory(&self) -> Arc<IslandMemory> {
        self.memory.clone()
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
        self.components.saturating_mul(INSTANCES_PER_COMPONENT)
    }

    fn tables(&self) -> usize {
        self.components.saturating_mul(MEMORIES_PER_COMPONENT)
    }

    fn memories(&self) -> usize {
        self.components.saturating_mul(MEMORIES_PER_COMPONENT)
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
    /// island. `contract` is the node's contract with its
    /// [`capabilities`](ComponentContract::capabilities) replaced by what
    /// the node's component actually imports: its world's imports that its
    /// code uses, minus those links satisfy, plus what providers composed
    /// in import themselves.
    fn link(
        &self,
        node: &NodeId,
        contract: &ComponentContract,
        linker: &mut Linker<Self::Data>,
    ) -> wasmtime::Result<()>;

    /// The data of a new Store for `members` (in island order), wrapping
    /// `state`. An island has one Store, or several when its members need
    /// not share one (see the [module docs](self)), so state kept in it is
    /// per Store, not per island (setting
    /// [`RuntimeConfig::split_islands`](crate::RuntimeConfig::split_islands)
    /// to `false` keeps every island in one Store);
    /// [`IslandData::host_state`] must return that very value for the
    /// Store's life (it carries the island's memory accounting, which the
    /// island's Stores share). Called when the island is built at load, and again every
    /// time it is rebuilt after a fault, cancel, shutdown or restore. An
    /// error, or a panic, fails the load ([`LoadError::Instantiation`]) or
    /// the rebuild ([`NodeFault::Restart`]).
    ///
    /// [`LoadError::Instantiation`]: crate::LoadError::Instantiation
    fn island_data(&self, members: &[NodeId], state: HostState) -> wasmtime::Result<Self::Data>;

    /// Whether the host can provide `capability` to `node`, asked for every
    /// capability every node's component imports (what its bytes import,
    /// which can be fewer than its contract declares) before anything is
    /// linked, and by `load_with_host` before anything is compiled. A gap
    /// fails the
    /// load with [`LoadError::MissingCapability`] or
    /// [`LoadError::AmbiguousCapability`], naming the node and the
    /// capability, instead of a linker error at instantiation.
    ///
    /// The default knows nothing and reports no gap: a capability
    /// [`link`](Self::link) leaves out then fails the node's instantiation
    /// ([`LoadError::Instantiation`]). [`Plugins`](crate::Plugins)
    /// answers from what its plugins declare.
    ///
    /// [`LoadError::MissingCapability`]: crate::LoadError::MissingCapability
    /// [`LoadError::AmbiguousCapability`]: crate::LoadError::AmbiguousCapability
    /// [`LoadError::Instantiation`]: crate::LoadError::Instantiation
    fn check(&self, node: &NodeId, capability: &Capability) -> Result<(), CapabilityGap> {
        let _ = (node, capability);
        Ok(())
    }
}

/// Why a host cannot provide a capability ([`Host::check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityGap {
    /// Nothing provides it.
    Missing,
    /// Several providers could, and nothing picks one: their ids.
    Ambiguous(Vec<String>),
}

/// The host of a graph whose components import no capabilities. A
/// component that does fails the load with
/// [`LoadError::MissingCapability`](crate::LoadError::MissingCapability).
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

    /// It provides nothing. (Lowering never lists an interface imported
    /// only for its types as a capability.)
    fn check(&self, _: &NodeId, _: &Capability) -> Result<(), CapabilityGap> {
        Err(CapabilityGap::Missing)
    }
}

/// Creates the engine every island of one graph shares.
///
/// Besides the async ABI, it enables every component-model value type that
/// lowering lets through in a capability's signature (`map`,
/// `error-context`, fixed-length lists) and labelled interface imports
/// (`import primary: clock;`), so a world that lowers also loads. Port
/// payloads are narrower: lowering rejects those types there.
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
        .wasm_component_model_implements(true)
        .concurrency_support(true)
        .consume_fuel(true);
    Engine::new(&config)
}

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
    let host_interface = format!(
        "{}@{}",
        witgraph_wit::lower::HOST_INTERFACE,
        witgraph_wit::lower::HOST_VERSION
    );
    linker.instance(&host_interface)?.func_wrap(
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

/// What instantiating one node needs: its component, pre-linked once
/// against its linker (the built-in host interface plus the embedder's
/// capabilities), and the name its `node` interface is exported under.
pub(crate) struct NodeBinary<D: 'static> {
    pub(crate) pre: InstancePre<D>,
    /// `node` for an inline interface, the interface's full id for a named
    /// one (see `witgraph_wit::lower::Lowered::export`).
    pub(crate) export: String,
    /// How many components an instance holds: the node's, and one per
    /// provider composed in. The Store's item limits scale with it.
    pub(crate) components: usize,
}

/// A live island's wasmtime side: its Stores, and each member's `run`
/// export in the Store it is instantiated in. The island's scheduling
/// state is the scheduler's.
///
/// Members joined by a stream the host cannot move (see [`crate::pump`]),
/// or by a future, share a Store, so the handle passes straight from one
/// to the other. Every other member has a Store to itself.
pub(crate) struct IslandStore<D: 'static> {
    cells: Vec<Cell<D>>,
    /// Per member, in plan order: its cell, and its position in it.
    place: Vec<(usize, usize)>,
}

/// One Store of an island, and the members instantiated in it.
struct Cell<D: 'static> {
    store: Store<D>,
    /// The members, by their position in the island plan.
    members: Vec<usize>,
    /// Each member's `run` export.
    runs: Vec<Func>,
    /// Each member's stream outputs: the element type of each, by port.
    streams: Vec<HashMap<String, Type>>,
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

/// Instantiates every member of an island into fresh Stores.
///
/// `members` pairs each member's node with what instantiating it needs, in
/// the island plan's order. `cells` says which members share a Store (see
/// [`IslandStore`]): each lists positions in `members`, and `data` holds
/// the data of each cell's Store. The signatures are in member order.
pub(crate) async fn build_island<D: IslandData>(
    engine: &Engine,
    members: &[(&NodeId, &NodeBinary<D>)],
    cells: &[Vec<usize>],
    data: Vec<D>,
    settings: StoreSettings,
) -> Result<(IslandStore<D>, Vec<RunSignature>), BuildError> {
    let first = members
        .first()
        .map_or_else(|| NodeId::from("?"), |(node, _)| (*node).clone());
    let plain = |node: &NodeId, message: String| BuildError {
        node: node.clone(),
        message,
        fatal: None,
    };
    let mut built = Vec::with_capacity(cells.len());
    let mut place = vec![(0, 0); members.len()];
    let mut signatures: Vec<Option<RunSignature>> = members.iter().map(|_| None).collect();
    for (index, (cell, data)) in cells.iter().zip(data).enumerate() {
        let mut store = Store::new(engine, data);
        store.limiter(|data: &mut D| data.host_state() as &mut dyn wasmtime::ResourceLimiter);
        // Instantiation (a guest's `_initialize`, say) gets a budget of its
        // own, as big as a `run`'s: a spinning start function faults
        // instead of hanging the load. Every `run` resets the fuel again.
        let setup = |store: &mut Store<D>| -> wasmtime::Result<()> {
            store.fuel_async_yield_interval(Some(settings.yield_interval))?;
            store.set_fuel(settings.budget())?;
            store.set_hostcall_fuel(settings.hostcall_fuel);
            Ok(())
        };
        // A failure of the Store itself is pinned on its first member.
        let owner = cell
            .first()
            .and_then(|member| members.get(*member))
            .map_or(&first, |(node, _)| *node);
        setup(&mut store).map_err(|e| plain(owner, format!("{e:#}")))?;

        let mut runs = Vec::with_capacity(cell.len());
        let mut streams = Vec::with_capacity(cell.len());
        for (slot, &member) in cell.iter().enumerate() {
            let Some((node, binary)) = members.get(member) else {
                return Err(plain(&first, format!("the island has no member {member}")));
            };
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
            let signature = signature(&store, run);
            streams.push(
                signature
                    .outputs
                    .iter()
                    .filter_map(|(name, ty)| match ty {
                        Type::Stream(stream) => Some((name.clone(), stream.ty()?)),
                        _ => None,
                    })
                    .collect(),
            );
            signatures[member] = Some(signature);
            place[member] = (index, slot);
            runs.push(run);
        }
        store
            .set_fuel(settings.budget())
            .map_err(|e| plain(owner, format!("{e:#}")))?;
        built.push(Cell {
            store,
            members: cell.clone(),
            runs,
            streams,
        });
    }
    let signatures = signatures
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| plain(&first, "a member of the island is in no Store".into()))?;
    Ok((
        IslandStore {
            cells: built,
            place,
        },
        signatures,
    ))
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

/// Runs one generation of `island`.
pub(crate) async fn run_generation<D: IslandData>(
    mut island: IslandStore<D>,
    mut generation: Generation<Val>,
    settings: StoreSettings,
) -> Outcome<IslandStore<D>> {
    for cell in &mut island.cells {
        cell.store.data_mut().host_state().fatal = None;
    }
    let budget = settings.budget();
    let external = std::mem::take(&mut generation.external);
    // One Store needs no channels between Stores: its members are called
    // straight through its accessor.
    let failed = match island.cells.as_mut_slice() {
        [cell] => run_cell(cell, &generation, external, budget)
            .await
            .map_err(|error| (error, Some(0))),
        _ => run_cells(&mut island, &generation, external, budget).await,
    };
    match failed {
        Ok(()) => Ok(island),
        Err((error, index)) => {
            // A `fatal` anywhere in the island wins over the error that
            // ended the generation: another Store may have failed first.
            let mut fatal = index
                .and_then(|index| island.cells.get_mut(index))
                .and_then(|cell| cell.store.data_mut().host_state().fatal.take());
            for cell in &mut island.cells {
                let state = cell.store.data_mut().host_state();
                if fatal.is_none() {
                    fatal = state.fatal.take();
                }
            }
            if fatal.is_some() {
                return Err(classify(&error, fatal));
            }
            let Some(cell) = index.and_then(|index| island.cells.get_mut(index)) else {
                return Err(classify(&error, None));
            };
            let (fault, culprit) = classify(&error, None);
            let culprit = culprit.or_else(|| {
                pinned_on(&fault, &cell.members, island.place.len())
                    .and_then(|member| generation.plan.members.get(member))
                    .map(|member| member.node.clone())
            });
            Err((fault, culprit))
        }
    }
}

/// The member a fault of a Store holding `cell` (of an island of
/// `members` members) is pinned on: a Store of one member pins it on that
/// member. In a larger one a member that returned may still be running a
/// task it spawned, so none is singled out. Nor is one for the memory
/// limit, which the island's Stores share: the Store that crossed it need
/// not hold most of it. An island of one is the scheduler's to name.
fn pinned_on(fault: &NodeFault, cell: &[usize], members: usize) -> Option<usize> {
    match cell {
        [only] if members > 1 && !matches!(fault, NodeFault::MemoryLimit { .. }) => Some(*only),
        _ => None,
    }
}

/// A generation of an island that is one Store: every member is called
/// through the Store's accessor, and stream and future handles pass
/// straight from one to the next.
async fn run_cell<D: IslandData>(
    cell: &mut Cell<D>,
    generation: &Generation<Val>,
    external: Vec<Vec<Option<Val>>>,
    budget: u64,
) -> wasmtime::Result<()> {
    let plan = &generation.plan;
    let runs = &cell.runs;
    cell.store
        .run_concurrent(async |acc| -> wasmtime::Result<()> {
            let members = Members {
                acc,
                plan,
                runs,
                budget,
            };
            let send = |kind| generation.send(kind);
            drive(&members, plan, external, option_payload, &send).await?;
            poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
            Ok(())
        })
        .await?
}

/// What moves between the members of an island of several Stores.
#[derive(Clone)]
enum CellValue {
    /// A value, or a handle that stays in the Store it was made in.
    Local(Val),
    /// A stream on its way to another Store: the receiving end of its pump,
    /// taken by the one member that reads it.
    Pumped(Arc<Mutex<Option<StreamRx>>>),
    /// A stream or future nothing in the island reads: already closed in
    /// the Store it was made in.
    Closed,
}

/// A call's reply could not be had: the call failed, or its Store's
/// generation ended, and the error itself is the Store's; or, with a
/// message, the driver never made the call.
struct CallFailed(Option<String>);

/// One call of a member's `run`, asked of the Store the member is in.
struct Request {
    member: usize,
    args: Vec<Option<CellValue>>,
    reply: oneshot::Sender<Result<Vec<(String, CellValue)>, CallFailed>>,
}

/// The members of an island of several Stores, as the generation driver
/// calls them: each call is sent to the member's Store and awaited.
struct Remote<'a> {
    cells: Vec<mpsc::UnboundedSender<Request>>,
    place: &'a [(usize, usize)],
    plan: &'a IslandPlan,
}

impl NodeCaller<CellValue> for Remote<'_> {
    type Error = CallFailed;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<CellValue>>,
    ) -> Result<Vec<(String, CellValue)>, CallFailed> {
        let (reply, answer) = oneshot::channel();
        let cell = self
            .place
            .get(member)
            .and_then(|(cell, _)| self.cells.get(*cell))
            .ok_or(CallFailed(None))?;
        cell.unbounded_send(Request {
            member,
            args,
            reply,
        })
        .map_err(|_| CallFailed(None))?;
        answer.await.map_err(|_| CallFailed(None))?
    }

    /// Nothing to do: an unread output is closed where it was made.
    fn close(&self, _: CellValue) -> Result<(), CallFailed> {
        Ok(())
    }

    fn missing_input(&self, member: usize, field: &PortName) -> CallFailed {
        CallFailed(Some(missing_input_message(self.plan, member, field)))
    }
}

/// What a connection that unwraps an option delivers for `value`, between
/// the Stores of an island.
fn cell_option_payload(value: &CellValue) -> OptionPayload<'_, CellValue> {
    match value {
        CellValue::Local(val) => match option_payload(val) {
            OptionPayload::Some(payload) => {
                OptionPayload::Some(Cow::Owned(CellValue::Local(payload.into_owned())))
            }
            OptionPayload::NotOption => OptionPayload::NotOption,
            OptionPayload::None => OptionPayload::None,
        },
        _ => OptionPayload::NotOption,
    }
}

/// A generation of an island of several Stores. Each Store serves the
/// calls of its own members, all Stores running at once; the driver asks
/// them in dependency order. A stream output read in another Store is
/// pumped there ([`crate::pump`]).
///
/// On failure, returns the error with the index of the cell it came from,
/// if it came from one.
async fn run_cells<D: IslandData>(
    island: &mut IslandStore<D>,
    generation: &Generation<Val>,
    external: Vec<Vec<Option<Val>>>,
    budget: u64,
) -> Result<(), (wasmtime::Error, Option<usize>)> {
    let IslandStore { cells, place } = island;
    let place = place.as_slice();
    let plan = &generation.plan;
    let mut senders = Vec::with_capacity(cells.len());
    let mut workers = FuturesUnordered::new();
    for (index, cell) in cells.iter_mut().enumerate() {
        let (tx, rx) = mpsc::unbounded();
        senders.push(tx);
        workers.push(
            serve_cell(cell, index, generation, place, rx, budget).map(move |done| (index, done)),
        );
    }
    let external = external
        .into_iter()
        .map(|fields| {
            fields
                .into_iter()
                .map(|v| v.map(CellValue::Local))
                .collect()
        })
        .collect();
    // Dropping the driver drops the request channels, which lets every
    // Store finish its generation.
    let driver = async move {
        let members = Remote {
            cells: senders,
            place,
            plan,
        };
        let send = |kind| generation.send(local_event(kind));
        drive(&members, plan, external, cell_option_payload, &send).await
    }
    .fuse();
    futures::pin_mut!(driver);
    let mut calls_failed = None;
    loop {
        futures::select! {
            driven = driver => match driven {
                // The driver's own failure: no call is to be made, so the
                // Stores are stopped (dropped with their generations) now.
                Err(CallFailed(Some(message))) => {
                    return Err((wasmtime::format_err!("{message}"), None));
                }
                driven => calls_failed = driven.err(),
            },
            (index, done) = workers.select_next_some() => {
                if let Err(error) = done {
                    return Err((error, Some(index)));
                }
            }
            complete => break,
        }
    }
    match calls_failed {
        // A failed call's Store reports the error itself; this is reached
        // only if none did, so no Store is to blame.
        Some(CallFailed(_)) => Err((wasmtime::format_err!("a call got no reply"), None)),
        None => Ok(()),
    }
}

/// An event of a generation, as the scheduler takes it:
/// Value outputs are plain values.
fn local_event(kind: IslandEventKind<CellValue>) -> IslandEventKind<Val> {
    kind.filter_map_values(|value| match value {
        CellValue::Local(val) => Some(val),
        _ => None,
    })
}

/// Serves one Store's share of a generation: calls its members as the
/// driver asks, concurrently, until the driver is done; then waits for the
/// streams it pumps out to end and for the guest work left in the Store,
/// and cuts the streams it took in ([`crate::pump`]).
async fn serve_cell<D: IslandData>(
    cell: &mut Cell<D>,
    index: usize,
    generation: &Generation<Val>,
    place: &[(usize, usize)],
    mut requests: mpsc::UnboundedReceiver<Request>,
    budget: u64,
) -> wasmtime::Result<()> {
    let Cell {
        store,
        runs,
        streams,
        ..
    } = cell;
    store
        .run_concurrent(async |acc| -> wasmtime::Result<()> {
            let served = Served {
                acc,
                index,
                generation,
                place,
                runs,
                streams,
                budget,
                pumped: Mutex::new(Vec::new()),
                intakes: Mutex::new(Vec::new()),
            };
            let mut calls = FuturesUnordered::new();
            loop {
                futures::select! {
                    done = calls.select_next_some() => done?,
                    request = requests.next() => match request {
                        Some(request) => calls.push(served.serve(request)),
                        None => break,
                    },
                }
            }
            while let Some(done) = calls.next().await {
                done?;
            }
            drop(calls);
            // A pumped stream's writer need not be a guest task (a node may
            // return a stream it was given), so the Store keeps forwarding
            // until each pump has ended.
            let pumped = std::mem::take(&mut *lock(&served.pumped)?);
            poll_fn(|cx| {
                // Every pump is polled, so each registers its waker.
                pumped.iter().fold(Poll::Ready(()), |all, ended| {
                    if ended.poll(cx).is_ready() {
                        all
                    } else {
                        Poll::Pending
                    }
                })
            })
            .await;
            poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
            for intake in std::mem::take(&mut *lock(&served.intakes)?) {
                intake.cut();
            }
            Ok(())
        })
        .await?
}

/// Locks `mutex`, which no panic holds.
fn lock<T>(mutex: &Mutex<T>) -> wasmtime::Result<std::sync::MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| wasmtime::format_err!("a lock was poisoned"))
}

/// One Store of an island of several, while it serves a generation.
struct Served<'a, D: IslandData> {
    acc: &'a Accessor<D>,
    /// The Store's cell.
    index: usize,
    generation: &'a Generation<Val>,
    place: &'a [(usize, usize)],
    runs: &'a [Func],
    streams: &'a [HashMap<String, Type>],
    budget: u64,
    /// The pumps this Store's members' outputs went out through.
    pumped: Mutex<Vec<Arc<Ended>>>,
    /// The pumps this Store's members' inputs came in through.
    intakes: Mutex<Vec<Intake>>,
}

impl<D: IslandData> Served<'_, D> {
    /// Answers one request. A failed call is this Store's failure: the
    /// driver only learns that there is no reply.
    async fn serve(&self, request: Request) -> wasmtime::Result<()> {
        let Request {
            member,
            args,
            reply,
        } = request;
        match self.call(member, args).await {
            Ok(outputs) => {
                let _ = reply.send(Ok(outputs));
                Ok(())
            }
            Err(error) => {
                let _ = reply.send(Err(CallFailed(None)));
                Err(error)
            }
        }
    }

    /// Calls a member's `run`: streams pumped from other Stores become
    /// streams of this one first, and stream outputs read in other Stores
    /// are pumped out afterwards.
    async fn call(
        &self,
        member: usize,
        args: Vec<Option<CellValue>>,
    ) -> wasmtime::Result<Vec<(String, CellValue)>> {
        let plan = self
            .generation
            .plan
            .members
            .get(member)
            .ok_or_else(|| wasmtime::format_err!("the island has no member {member}"))?;
        let slot = match self.place.get(member) {
            Some(&(cell, slot)) if cell == self.index => slot,
            _ => wasmtime::bail!("`{}` is not in this Store", plan.node),
        };
        let args = args
            .into_iter()
            .map(|arg| match arg {
                Some(CellValue::Local(val)) => Ok(Some(val)),
                Some(CellValue::Pumped(rx)) => {
                    let rx = rx.lock().ok().and_then(|mut rx| rx.take());
                    let rx = rx.ok_or_else(|| wasmtime::format_err!("a stream was read twice"))?;
                    let (val, intake) = import_stream(self.acc, rx)?;
                    lock(&self.intakes)?.push(intake);
                    Ok(Some(val))
                }
                Some(CellValue::Closed) | None => Ok(None),
            })
            .collect::<wasmtime::Result<Vec<_>>>()?;
        let run = self
            .runs
            .get(slot)
            .ok_or_else(|| wasmtime::format_err!("`{}` has no `run`", plan.node))?;
        call_run(self.acc, run, plan, args, self.budget)
            .await?
            .into_iter()
            .map(|(name, val)| {
                let value = self.output(member, slot, plan, &name, val)?;
                Ok((name, value))
            })
            .collect()
    }

    /// What an output of a member becomes on its way to its readers.
    fn output(
        &self,
        member: usize,
        slot: usize,
        plan: &MemberPlan,
        name: &str,
        val: Val,
    ) -> wasmtime::Result<CellValue> {
        // Found by name without allocating a key: a member has few outputs.
        let Some((port, output)) = plan.outputs.iter().find(|(port, _)| port.as_str() == name)
        else {
            return Ok(CellValue::Local(val));
        };
        if output.kind == PortKind::Value {
            return Ok(CellValue::Local(val));
        }
        // A stream or future has one reader in the island, or none.
        let reader = output.consumers.first().map(|consumer| consumer.member);
        let here = |member: usize| {
            self.place
                .get(member)
                .is_some_and(|(cell, _)| *cell == self.index)
        };
        match (reader, val) {
            (None, val) => {
                close(self.acc, val)?;
                Ok(CellValue::Closed)
            }
            (Some(reader), val) if here(reader) => Ok(CellValue::Local(val)),
            (Some(_), Val::Stream(stream)) => {
                let element = self
                    .streams
                    .get(slot)
                    .and_then(|streams| streams.get(name))
                    .ok_or_else(|| {
                        wasmtime::format_err!("stream `{}.{name}` has no element type", plan.node)
                    })?;
                let tap = self.generation.stream_items.then(|| Tap {
                    reporter: self.generation.reporter(),
                    member,
                    port: port.clone(),
                });
                let (rx, ended) = export_stream(self.acc, stream, element, tap)?;
                lock(&self.pumped)?.push(ended);
                Ok(CellValue::Pumped(Arc::new(Mutex::new(Some(rx)))))
            }
            (Some(_), _) => Err(wasmtime::format_err!(
                "`{}.{name}` cannot leave its Store",
                plan.node
            )),
        }
    }
}

/// Rebuilds a stopped island into fresh Stores, then runs one generation
/// in them. Both are the generation: the rebuild runs inside the
/// generation's future, beside every other island, and survives a dropped
/// tick. A failed rebuild is the fault [`NodeFault::Restart`], or
/// [`NodeFault::Fatal`] when a member's start function called `fatal`.
pub(crate) async fn rebuild_and_run<D: IslandData>(
    engine: Engine,
    members: IslandBinaries<D>,
    cells: Vec<Vec<usize>>,
    data: Vec<wasmtime::Result<D>>,
    generation: Generation<Val>,
    settings: StoreSettings,
) -> Outcome<IslandStore<D>> {
    let parts: Vec<(&NodeId, &NodeBinary<D>)> = members
        .iter()
        .map(|(node, binary)| (node, &**binary))
        .collect();
    let data = match data.into_iter().collect::<wasmtime::Result<Vec<D>>>() {
        Ok(data) => data,
        Err(e) => {
            let message = format!("island data: {e:#}");
            return Err((NodeFault::Restart { message }, None));
        }
    };
    let island = match build_island(&engine, &parts, &cells, data, settings).await {
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
    generation.send(IslandEventKind::Rebuilt);
    run_generation(island, generation, settings).await
}

/// The members of an island of one Store, as the generation driver calls
/// them: each `run` export, called through the Store's accessor.
struct Members<'a, D: IslandData> {
    acc: &'a Accessor<D>,
    plan: &'a IslandPlan,
    runs: &'a [Func],
    /// The fuel level set before every `run` call.
    budget: u64,
}

impl<D: IslandData> NodeCaller<Val> for Members<'_, D> {
    type Error = wasmtime::Error;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<Val>>,
    ) -> wasmtime::Result<Vec<(String, Val)>> {
        call_run(
            self.acc,
            &self.runs[member],
            &self.plan.members[member],
            args,
            self.budget,
        )
        .await
    }

    fn close(&self, val: Val) -> wasmtime::Result<()> {
        close(self.acc, val)
    }

    fn missing_input(&self, member: usize, field: &PortName) -> wasmtime::Error {
        wasmtime::format_err!("{}", missing_input_message(self.plan, member, field))
    }
}

/// Calls a member's `run` with its collected inputs, after resetting the
/// Store's fuel to `budget`, and returns its outputs by field name.
async fn call_run<D: IslandData>(
    acc: &Accessor<D>,
    run: &Func,
    plan: &MemberPlan,
    args: Vec<Option<Val>>,
    budget: u64,
) -> wasmtime::Result<Vec<(String, Val)>> {
    let params = params(plan, args)?;
    acc.with(|mut access| access.as_context_mut().set_fuel(budget))?;
    let mut results = if plan.has_result {
        vec![Val::Bool(false)]
    } else {
        Vec::new()
    };
    run.call_concurrent(acc, &params, &mut results).await?;
    Ok(match results.into_iter().next() {
        Some(Val::Record(fields)) => fields,
        _ => Vec::new(),
    })
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

/// Builds `run`'s parameter list from a member's collected inputs, one per
/// field of its `inputs` record. [`drive`] has checked that every required
/// input has a value.
fn params(member: &MemberPlan, args: Vec<Option<Val>>) -> wasmtime::Result<Vec<Val>> {
    let Some(fields) = &member.inputs else {
        return Ok(Vec::new());
    };
    let mut record = Vec::with_capacity(fields.len());
    let mut args = args.into_iter();
    for field in fields {
        let value = args.next().flatten();
        let value = match (field.optional, value) {
            (true, value) => Val::Option(value.map(Box::new)),
            (false, Some(value)) => value,
            (false, None) => wasmtime::bail!("`{}.{}` has no value", member.node, field.name),
        };
        record.push((field.name.to_string(), value));
    }
    Ok(vec![Val::Record(record)])
}

/// An island's members, in island order, with what instantiating each
/// needs.
pub(crate) type IslandBinaries<D> = Vec<(NodeId, Arc<NodeBinary<D>>)>;

/// The wasmtime executor: runs each island of a graph in a Store of its
/// own, with the embedder's [`Host`] providing capabilities and Store data.
pub(crate) struct Wasmtime<H: Host> {
    pub(crate) host: H,
    pub(crate) engine: Engine,
    pub(crate) settings: StoreSettings,
    /// What a new island Store's memory is limited to.
    pub(crate) max_island_memory: Option<usize>,
    /// Per island, in island order: its members with what instantiating
    /// each needs, for rebuilds.
    pub(crate) binaries: Vec<IslandBinaries<H::Data>>,
    /// Per island: which of its members share a Store (see
    /// [`IslandStore`]), as positions in the island.
    pub(crate) cells: Vec<Vec<Vec<usize>>>,
    /// Payload type of every Value input port (inner type when optional).
    pub(crate) input_types: HashMap<PortRef, Type>,
    /// Type of every Value output port.
    pub(crate) output_types: HashMap<PortRef, Type>,
}

/// The data of a new Store for each of an island's `cells` (positions in
/// `members`, the island's), all charged against one `memory_limit`.
pub(crate) fn island_data<H: Host>(
    host: &H,
    memory_limit: Option<usize>,
    members: &[(&NodeId, &NodeBinary<H::Data>)],
    cells: &[Vec<usize>],
) -> Vec<wasmtime::Result<H::Data>> {
    let components = cells.iter().map(|cell| {
        cell.iter()
            .filter_map(|member| members.get(*member))
            .map(|(_, binary)| binary.components)
            .sum()
    });
    let states = HostState::island(memory_limit, components);
    cells
        .iter()
        .zip(states)
        .map(|(cell, state)| {
            let nodes: Vec<NodeId> = cell
                .iter()
                .filter_map(|member| members.get(*member))
                .map(|(node, _)| (*node).clone())
                .collect();
            // The embedder's code: a panic in it fails this Store (a rebuild
            // then faults with `Restart`) rather than unwinding through the
            // scheduler while it swaps the island's state.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                host.island_data(&nodes, state)
            }))
            .unwrap_or_else(|_| Err(wasmtime::format_err!("`Host::island_data` panicked")))
        })
        .collect()
}

impl<H: Host> Executor for Wasmtime<H> {
    type Value = Val;
    type Type = Type;
    type Island = IslandStore<H::Data>;

    fn run(
        &self,
        island: Self::Island,
        generation: Generation<Val>,
    ) -> GenerationFuture<Self::Island> {
        run_generation(island, generation, self.settings).boxed()
    }

    fn rebuild_and_run(&self, generation: Generation<Val>) -> GenerationFuture<Self::Island> {
        let index = generation.plan.index;
        let members = self.binaries.get(index).cloned().unwrap_or_default();
        let cells = self.cells.get(index).cloned().unwrap_or_default();
        let parts: Vec<(&NodeId, &NodeBinary<H::Data>)> = members
            .iter()
            .map(|(node, binary)| (node, &**binary))
            .collect();
        let data = island_data(&self.host, self.max_island_memory, &parts, &cells);
        rebuild_and_run(
            self.engine.clone(),
            members,
            cells,
            data,
            generation,
            self.settings,
        )
        .boxed()
    }

    fn option_payload(value: &Val) -> OptionPayload<'_, Val> {
        option_payload(value)
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&Type> {
        match direction {
            PortDirection::Input => self.input_types.get(port),
            PortDirection::Output => self.output_types.get(port),
        }
    }

    fn check_input(&self, ty: &Type, value: Val) -> Result<Val, String> {
        check_type(ty, &value)?;
        Ok(canonical(ty, value))
    }

    fn parse_wave(&self, ty: &Type, text: &str) -> Result<Val, String> {
        parse_wave(ty, text)
    }

    /// Rendering into a `String` cannot fail, and a Value port never holds
    /// a kind WAVE cannot render (a handle, say).
    fn to_wave(&self, value: &Val) -> String {
        value.to_wave().unwrap_or_default()
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
    // A pump's charge arrives wrapped in the failed write's error.
    if let Some(exceeded) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MemoryLimitExceeded>())
    {
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

/// What a connection that unwraps an option delivers for `val`.
pub(crate) fn option_payload(val: &Val) -> OptionPayload<'_, Val> {
    match val {
        Val::Option(Some(payload)) => OptionPayload::Some(Cow::Borrowed(payload)),
        Val::Option(None) => OptionPayload::None,
        _ => OptionPayload::NotOption,
    }
}

/// Parses WAVE text as a value of type `ty`, in [`canonical`] form. Unlike
/// WAVE's own parser, it rejects record fields, cases and flags the type
/// does not have, and fields given twice.
pub(crate) fn parse_wave(ty: &Type, text: &str) -> Result<Val, String> {
    let val: Val = witgraph_ir::wave::parse(ty, text)?;
    Ok(canonical(ty, val))
}

/// Checks that `val` has type `ty`, as every executor does
/// ([`witgraph_ir::wave::check_value`]).
pub(crate) fn check_type(ty: &Type, val: &Val) -> Result<(), String> {
    witgraph_ir::wave::check_value(ty, val)
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
            components: 1,
        };
        let settings = StoreSettings {
            yield_interval: 100_000,
            fuel_per_run: None,
            hostcall_fuel: 128 << 20,
        };
        let (_, signatures) = build_island(
            &engine,
            &[(&node, &binary)],
            &[vec![0]],
            HostState::island(None, [1]),
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
        let mut states = HostState::island(Some(64 * 1024), [2, 1]);
        let mut state = states.remove(0);
        assert!(state.memory_growing(0, 65536, None).unwrap());
        assert!(
            state.table_growing(0, 16 << 20, None).is_err(),
            "16M table elements are far over 64 KiB"
        );
        // A grow past the memory's own maximum fails, and costs nothing.
        assert!(!state.memory_growing(65536, 131072, Some(65536)).unwrap());
        assert_eq!(state.memory.used.load(Ordering::Relaxed), 65536);
        // A failure wasmtime reports refunds nothing: it could be forged.
        state
            .memory_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        state
            .table_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        assert_eq!(state.memory.used.load(Ordering::Relaxed), 65536);
        let err = state.memory_growing(65536, 131072, None).unwrap_err();
        assert!(err.downcast_ref::<MemoryLimitExceeded>().is_some());
        // The island's other Store draws on the same limit.
        let mut other = states.remove(0);
        let err = other.memory_growing(0, 1, None).unwrap_err();
        assert!(err.downcast_ref::<MemoryLimitExceeded>().is_some());
        assert_eq!(other.instances(), INSTANCES_PER_COMPONENT);
        assert_eq!(state.instances(), 2 * INSTANCES_PER_COMPONENT);
        assert_eq!(state.memories(), 2 * MEMORIES_PER_COMPONENT);
        assert_eq!(state.tables(), 2 * MEMORIES_PER_COMPONENT);
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
        assert_eq!(
            option_payload(&some),
            OptionPayload::Some(Cow::Borrowed(&Val::U32(5)))
        );
        assert_eq!(option_payload(&Val::Option(None)), OptionPayload::None);
        assert_eq!(option_payload(&Val::U32(5)), OptionPayload::NotOption);
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

    #[test]
    fn a_fault_is_pinned_only_on_the_one_member_of_its_store() {
        let trap = NodeFault::WasmTrap {
            message: "boom".into(),
        };
        let memory = NodeFault::MemoryLimit {
            requested: 2,
            limit: 1,
        };
        // A Store of one member, in an island of several Stores.
        assert_eq!(pinned_on(&trap, &[2], 3), Some(2));
        assert_eq!(pinned_on(&NodeFault::FuelExhausted, &[2], 3), Some(2));
        // The Stores share the memory limit.
        assert_eq!(pinned_on(&memory, &[2], 3), None);
        // A Store of several members.
        assert_eq!(pinned_on(&trap, &[0, 1], 3), None);
        // An island of one: the scheduler names its member.
        assert_eq!(pinned_on(&trap, &[0], 1), None);
    }

    #[tokio::test]
    async fn values_are_checked_structurally() {
        let ty = echo_in_type().await;
        assert!(check_type(&ty, &Val::Float64(1.0)).is_ok());
        let err = check_type(&ty, &Val::U32(1)).unwrap_err();
        assert_eq!(err, "expected f64, found u32");
    }
}
