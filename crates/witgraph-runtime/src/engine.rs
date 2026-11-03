//! Wasmtime plumbing: the engine, the graph's Store, and what an embedder
//! provides.
//!
//! # One composite, one Store
//!
//! A graph is one composed component ([`witgraph_wit::hub`]): a generated hub,
//! holding the latched values in its linear memory, and an instance of every
//! node. So values pass from node to node inside the Store, and the host reads
//! one only when it asks. That also makes the Store the fault domain: any trap,
//! a spawned task's included, stops the whole graph. See
//! [`crate::graph`] for how it is driven.
//!
//! # Handles
//!
//! A handle port's object ([`Shared`]) is the host's, kept in the Store's
//! [`HostState`]. The hub holds a guest's `own` handle to it in its slot and
//! lends readers a `borrow`, so an object lives while a slot, or a guest's own
//! reference, holds it ([`crate::shared`]). The host is lent one when it asks.
//!
//! # Embedding
//!
//! A Store's data is the embedder's [`Host::Data`], which carries witgraph's own
//! [`HostState`] ([`StoreData`]). The [`Host`] links the graph's capability
//! imports and creates the data of the Store, on load and on every rebuild, so
//! capability state can live in it.
//!
//! # Sandboxing
//!
//! The Store meters fuel. `fuel_async_yield_interval` makes it yield to the
//! executor every `yield_interval` units, so a timeout around a tick can fire;
//! nothing else runs in the Store until the guest suspends, and that includes the
//! host's reads and writes. Fuel is reset to the per-run budget before every `run`
//! call (the budget is shared by everything executing in the Store, spawned tasks
//! included). A Store that burns the budget before its next `run` starts traps, and
//! the graph faults with [`NodeFault::FuelExhausted`]; that includes an endless
//! stream once it has consumed the budget, so endless streams need an unlimited
//! (`None`) or suitably large budget.
//!
//! The Store caps the linear memory and tables it may hold (`max_memory`), how
//! many instances, memories and tables it may hold (in proportion to the components
//! composed into it, providers included), and the host memory the values lifted out
//! of a guest in a single call may take (wasmtime's hostcall fuel). Exceeding any of
//! them faults the graph.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wasmtime::component::{Linker, LinkerInstance, ResourceDynamic, Type, Val};
use wasmtime::{Engine, StoreContextMut};
use witgraph_ir::{Capability, HandleType, NodeId, NodeIdx, OutputIdx, PortKind, PortTable};
use witgraph_sched::NodeFault;

use crate::shared::{Shared, SharedLimitExceeded, SharedMemory, SharedTable};

/// witgraph's own state in the Store: who called `fatal`, and its memory
/// accounting.
///
/// It has no public API and no public constructor: witgraph creates one per
/// Store and hands it to [`Host::store_data`], whose data must keep exactly that
/// value and return it from [`StoreData::host_state`] for the Store's whole life.
/// It carries the graph's memory limit, so replacing it is not possible from
/// outside the crate.
#[derive(Debug)]
pub struct HostState {
    /// The first `fatal` call in this Store: the calling node and message.
    pub(crate) fatal: Option<(NodeId, String)>,
    /// The memory the Store has been granted.
    used: AtomicUsize,
    /// The most `used` may grow to; `None` is unlimited.
    limit: Option<usize>,
    /// How many components are composed into the Store: what its item limits
    /// scale with.
    components: usize,
    /// The shared objects this Store's guests hold handles to.
    shared: SharedTable,
    /// The graph's shared objects' memory accounting.
    shared_memory: Arc<SharedMemory>,
    /// What the hub last lent the host.
    pub(crate) lent: Option<Shared>,
}

/// Bytes charged for one table element: a reference.
const TABLE_ELEMENT_BYTES: usize = 8;

/// The most core instances the Store may hold, per component composed into it.
/// A component built by wit-component has a handful.
pub(crate) const INSTANCES_PER_COMPONENT: usize = 64;

/// The most linear memories, and the most tables, the Store may hold, per
/// component composed into it. Every memory reserves address space up front
/// (`memory_reservation`) whatever its size, so their number is capped apart
/// from `max_memory`: a crafted component cannot exhaust the process's address
/// space with empty memories.
pub(crate) const MEMORIES_PER_COMPONENT: usize = 16;

/// Growing a linear memory or table would take the Store past `max_memory`.
#[derive(Debug)]
pub(crate) struct MemoryLimitExceeded {
    pub(crate) requested: usize,
    pub(crate) limit: usize,
}

impl std::fmt::Display for MemoryLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the graph needs {} bytes of memory, over its limit of {}",
            self.requested, self.limit
        )
    }
}

impl std::error::Error for MemoryLimitExceeded {}

impl HostState {
    /// Fresh state for a Store limited to `memory_limit` bytes, holding that
    /// many components.
    pub(crate) fn new(
        memory_limit: Option<usize>,
        shared_memory: &Arc<SharedMemory>,
        components: usize,
    ) -> Self {
        Self {
            fatal: None,
            used: AtomicUsize::new(0),
            limit: memory_limit,
            components: components.max(1),
            shared: SharedTable::default(),
            shared_memory: shared_memory.clone(),
            lent: None,
        }
    }

    /// Charges a growth of `bytes` against the limit.
    fn grow(&mut self, bytes: usize) -> wasmtime::Result<bool> {
        let limit = self.limit;
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let requested = used.saturating_add(bytes);
                limit
                    .is_none_or(|limit| requested <= limit)
                    .then_some(requested)
            })
            .map(|_| true)
            .map_err(|used| {
                wasmtime::Error::new(MemoryLimitExceeded {
                    requested: used.saturating_add(bytes),
                    limit: limit.unwrap_or(usize::MAX),
                })
            })
    }

    pub(crate) fn shared_table(&mut self) -> &mut SharedTable {
        &mut self.shared
    }

    /// Makes `object` a shared object of `resource`, declared in
    /// `interface` (linked with [`link_resource`](crate::shared::link_resource)),
    /// charged `bytes` against the graph's
    /// [`max_shared_memory`](crate::RuntimeConfig::max_shared_memory), and
    /// returns the guest's `own` handle to it: what a constructor or an
    /// operation making a new object returns. Over the limit, it fails, and
    /// so does the guest's call, as [`NodeFault::MemoryLimit`].
    pub fn share<T: std::any::Any + Send + Sync>(
        &mut self,
        interface: &str,
        resource: &str,
        object: T,
        bytes: usize,
    ) -> wasmtime::Result<ResourceDynamic> {
        let tag = crate::shared::tag(interface, resource);
        let shared = self
            .shared_memory
            .share(tag, object, bytes)
            .map_err(wasmtime::Error::new)?;
        self.share_again(shared)
    }

    /// A new `own` handle to an object that already exists: what an
    /// operation returning the same object, or a view of it, returns.
    pub fn share_again(&mut self, shared: Shared) -> wasmtime::Result<ResourceDynamic> {
        let tag = shared.tag();
        let rep = self.shared.insert(shared)?;
        Ok(crate::shared::own(rep, tag))
    }

    /// The object behind a handle a guest passed: an `own` or a `borrow`
    /// of a resource handle ports may carry.
    pub fn shared(&self, handle: &ResourceDynamic) -> wasmtime::Result<Shared> {
        self.shared
            .get(handle.rep())
            .filter(|shared| shared.tag() == handle.ty())
            .cloned()
            .ok_or_else(|| wasmtime::format_err!("no shared object behind this handle"))
    }
}

/// Linear memory and tables both count against `max_memory`, as
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

/// The data of the Store: the embedder's own state for its capabilities, plus
/// witgraph's [`HostState`].
pub trait StoreData: Send + 'static {
    /// witgraph's state in this Store.
    fn host_state(&mut self) -> &mut HostState;
}

impl StoreData for HostState {
    fn host_state(&mut self) -> &mut HostState {
        self
    }
}

/// What an embedder provides to a runtime graph: its capability imports, and the
/// data of its Store.
///
/// The Store data type is the embedder's, so capability state can live in it
/// (rebuilt with the graph) and implementations that need a particular data type
/// (WASI, `bindgen!` hosts) can be linked.
pub trait Host: Send + 'static {
    /// The data of the Store.
    type Data: StoreData;

    /// Adds the graph's capability imports to its linker, once, at load, after
    /// the built-in `witgraph:runtime/host` interface has been added; the linker
    /// is kept for every rebuild. `capabilities` is what the graph's components
    /// actually import: their worlds' imports their code uses, minus those links
    /// satisfy, plus what providers composed in import themselves. Nodes importing
    /// one name share one import, so resource types unify; two nodes wanting
    /// different providers of one interface use labels.
    fn link(
        &self,
        capabilities: &[Capability],
        linker: &mut Linker<Self::Data>,
    ) -> wasmtime::Result<()>;

    /// The data of a new Store, wrapping `state`.
    /// [`StoreData::host_state`] must return that very value for the Store's
    /// life (it carries the graph's memory accounting). Called when the graph is
    /// built at load, and again every time it is rebuilt after a fault, shutdown
    /// or restore. An error, or a panic, fails the load
    /// ([`LoadError::Instantiation`]) or the rebuild ([`NodeFault::Restart`]).
    ///
    /// [`LoadError::Instantiation`]: crate::LoadError::Instantiation
    fn store_data(&self, state: HostState) -> wasmtime::Result<Self::Data>;

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
    /// [`link`](Self::link) leaves out then fails the graph's instantiation
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

    /// Whether `handle`, a resource a handle port of `node` carries, is one
    /// the host shares between nodes: one it links with
    /// [`link_resource`](crate::shared::link_resource), whose objects it
    /// makes with [`HostState::share`] (see [`crate::shared`]). A graph
    /// with a handle port the host does not share fails to load
    /// ([`LoadError::NotShareable`](crate::LoadError::NotShareable)). The
    /// default shares nothing.
    fn shares(&self, node: &NodeId, handle: &HandleType) -> bool {
        let _ = (node, handle);
        false
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

    fn link(&self, _: &[Capability], _: &mut Linker<HostState>) -> wasmtime::Result<()> {
        Ok(())
    }

    fn store_data(&self, state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }

    /// It provides nothing. (Lowering never lists an interface imported
    /// only for its types as a capability.)
    fn check(&self, _: &NodeId, _: &Capability) -> Result<(), CapabilityGap> {
        Err(CapabilityGap::Missing)
    }
}

/// Creates the engine a graph's Store runs on.
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

/// A linker for a composed graph: each node's own `witgraph:runtime/host`
/// (whose `fatal` knows which node called it), what lends the host a handle,
/// and the host's capabilities.
pub(crate) fn graph_linker<H: Host>(
    engine: &Engine,
    ports: &PortTable,
    capabilities: &[Capability],
    host: &H,
) -> wasmtime::Result<Linker<H::Data>> {
    let mut linker = Linker::new(engine);
    // The composite imports each of these only if something uses it.
    linker.allow_shadowing(false);
    for position in 0..ports.node_count() {
        let caller = ports.node_id(NodeIdx::from_position(position)).clone();
        link_fatal::<H::Data>(
            &mut linker.instance(&witgraph_wit::hub::host_import(position))?,
            caller,
        )?;
    }
    let handles: Vec<OutputIdx> = ports
        .outputs()
        .iter_enumerated()
        .filter(|(_, entry)| entry.kind == PortKind::Handle)
        .map(|(output, _)| output)
        .collect();
    if !handles.is_empty() {
        let mut lend = linker.instance(witgraph_wit::hub::LEND_INTERFACE)?;
        for output in handles {
            let name = witgraph_wit::hub::lend_handle(output);
            lend.func_new(&name, move |mut store, _, params, _| {
                let Some(Val::Resource(resource)) = params.first() else {
                    wasmtime::bail!("a lent handle is a resource");
                };
                let handle = ResourceDynamic::try_from_resource_any(*resource, &mut store)?;
                let state = store.data_mut().host_state();
                let shared = state.shared(&handle)?;
                state.lent = Some(shared);
                Ok(())
            })?;
        }
    }
    host.link(capabilities, &mut linker)?;
    Ok(linker)
}

/// Defines `fatal`, which knows who `caller` is by which import it came through.
fn link_fatal<D: StoreData>(
    instance: &mut LinkerInstance<'_, D>,
    caller: NodeId,
) -> wasmtime::Result<()> {
    instance.func_wrap(
        "fatal",
        move |mut store: StoreContextMut<'_, D>, (message,): (String,)| {
            let state = store.data_mut().host_state();
            if state.fatal.is_none() {
                state.fatal = Some((caller.clone(), message.clone()));
            }
            Err::<(), _>(wasmtime::format_err!("fatal: {message}"))
        },
    )
}

/// Settings applied to the Store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoreSettings {
    /// Fuel between cooperative yields to the executor.
    pub(crate) yield_interval: u64,
    /// Fuel the Store may burn between two `run` starts; `None` is effectively
    /// unlimited.
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

/// What a failure of the Store is, as a fault, with the node that caused it when
/// that is known.
pub(crate) fn classify(
    error: &wasmtime::Error,
    fatal: Option<(NodeId, String)>,
) -> (NodeFault, Option<NodeId>) {
    if let Some((node, message)) = fatal {
        return (NodeFault::Fatal { message }, Some(node));
    }
    if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
        return (NodeFault::FuelExhausted, None);
    }
    // A provider's charge arrives wrapped in the failed host call's error.
    if let Some(exceeded) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SharedLimitExceeded>())
    {
        let SharedLimitExceeded { requested, limit } = *exceeded;
        return (NodeFault::MemoryLimit { requested, limit }, None);
    }
    // A guest's growth arrives wrapped in the failed instruction's error.
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
        let component =
            wasmtime::component::Component::new(&engine, test_components::ECHO.wasm).unwrap();
        let linker = Linker::new(&engine);
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(u64::MAX / 2).unwrap();
        store.fuel_async_yield_interval(Some(100_000)).unwrap();
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .unwrap();
        let node = instance.get_export_index(&mut store, None, "node").unwrap();
        let run = instance
            .get_export_index(&mut store, Some(&node), "run")
            .unwrap();
        let run = instance.get_func(&mut store, run).unwrap();
        let (_, inputs) = run.ty(&store).params().next().unwrap();
        let Type::Record(record) = inputs else {
            panic!("`run` takes a record");
        };
        let field = record.fields().next().unwrap();
        assert_eq!(field.name, "in");
        match field.ty {
            Type::Option(option) => option.ty(),
            other => panic!("`in` is optional, found {other:?}"),
        }
    }

    #[test]
    fn the_store_memory_limit_counts_tables_and_skips_failed_growth() {
        use wasmtime::ResourceLimiter;
        let mut state = HostState::new(Some(64 * 1024), &SharedMemory::new(None), 2);
        assert!(state.memory_growing(0, 65536, None).unwrap());
        assert!(
            state.table_growing(0, 16 << 20, None).is_err(),
            "16M table elements are far over 64 KiB"
        );
        // A grow past the memory's own maximum fails, and costs nothing.
        assert!(!state.memory_growing(65536, 131072, Some(65536)).unwrap());
        assert_eq!(state.used.load(Ordering::Relaxed), 65536);
        // A failure wasmtime reports refunds nothing: it could be forged.
        state
            .memory_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        state
            .table_grow_failed(wasmtime::format_err!("no"))
            .unwrap();
        assert_eq!(state.used.load(Ordering::Relaxed), 65536);
        let err = state.memory_growing(65536, 131072, None).unwrap_err();
        assert!(err.downcast_ref::<MemoryLimitExceeded>().is_some());
    }

    #[test]
    fn a_reservation_too_large_to_round_is_an_error() {
        assert!(new_engine(Some(u64::MAX)).is_err());
        assert!(new_engine(Some(0)).is_ok());
        assert!(new_engine(Some(1)).is_ok());
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
