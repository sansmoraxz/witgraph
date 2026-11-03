//! The `RuntimeGraph` typestate: a compiled graph loaded with WASM
//! components, ready for execution.
//!
//! Extends the typestate chain: `GraphBuilder -> Graph -> CompiledGraph ->
//! RuntimeGraph`.
//!
//! A `RuntimeGraph` is the witgraph scheduler ([`witgraph_sched`]) over the
//! wasmtime executor ([`crate::engine`]): the scheduler decides when each
//! island runs a generation and keeps the host-visible state; the executor
//! runs the generation in the island's Store. The scheduling model, with
//! its invariants, is described in [`witgraph_sched::scheduler`].

pub(crate) mod load;

use std::future::Future;

use wasmtime::Engine;
use wasmtime::component::{Type, Val};

use crate::shared::Shared;
use crate::value::Value;
use witgraph_ir::{CompiledGraph, NodeId, PortDirection, PortKind, PortName};
use witgraph_sched::{LatchReader, Scheduler};
use witgraph_traits::{OutputReader, RunningGraph, Snapshot, SnapshotError};

pub use self::load::PreparedComponent;
use crate::engine::{self, Host, NoCapabilities, StoreSettings, Wasmtime};
use witgraph_sched::{
    FaultReport, NodeState, Perf, PortVersion, RuntimeError, RuntimeMode, TickResult,
};
#[cfg(feature = "compile")]
pub use witgraph_wit::compose::LinkedProvider;

/// Configuration for a runtime graph.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Most generations one tick may start before it returns
    /// [`TickResult::StepLimitReached`]: a bound on how long one tick can
    /// keep starting work, not a loop detector. A tick that hits it latches
    /// no feedback, so feedback loops advance at most one iteration per
    /// tick; a driver bounds a loop that never settles by the number of
    /// ticks it runs.
    pub max_steps_per_tick: usize,
    /// Fuel an island burns before yielding to the executor (at least 1),
    /// so one busy island cannot starve the others, and a timeout around
    /// [`RuntimeGraph::tick`] fires even while a guest spins.
    pub yield_interval: u64,
    /// Fuel an island may burn between two `run` starts before it faults
    /// with [`NodeFault::FuelExhausted`](crate::NodeFault::FuelExhausted):
    /// the island's fuel is reset to this budget before every `run` call,
    /// and instantiating the island (start functions included) gets a
    /// budget of the same size. The budget is shared by everything
    /// executing in the island, spawned tasks included, so an endless
    /// streaming generation exhausts any finite budget eventually. An
    /// island whose members run in several Stores (see
    /// [islands](crate::engine#islands)) has this budget in each Store. `None`
    /// means effectively unlimited, instantiation included.
    pub fuel_per_run: Option<u64>,
    /// Bytes of linear memory and tables (8 bytes per element) an island's
    /// instances may hold in total, across all its Stores. Growing past it
    /// faults the island
    /// ([`NodeFault::MemoryLimit`](crate::NodeFault::MemoryLimit)). `None`
    /// is unlimited. An island may also hold at most 16 memories, 16
    /// tables and 64 core instances per component it instantiates (a node,
    /// and each provider composed into it). Stream items the host holds
    /// while it pumps them between an island's Stores count against it
    /// too; other host-side copies of values do not (see
    /// [`hostcall_fuel`](Self::hostcall_fuel)).
    pub max_island_memory: Option<usize>,
    /// The most memory the graph's shared objects, the resources its
    /// handle ports carry ([`shared`](crate::shared)), may take in total,
    /// as their providers report their sizes (host or device memory).
    /// Making one past it fails the call that makes it, which faults the
    /// node's island
    /// ([`NodeFault::MemoryLimit`](crate::NodeFault::MemoryLimit)); an
    /// object's size is given back when the last holder drops it. `None` is
    /// unlimited; the default is 4 GiB.
    pub max_shared_memory: Option<usize>,
    /// Wasmtime's hostcall fuel, per call: roughly the bytes of host memory
    /// the values lifted *out of* a guest in one call may take, where every
    /// value element costs about 48 bytes. It bounds what a `run` returns,
    /// the arguments a guest passes to an imported function (a capability,
    /// or `fatal`), and stream and future items copied between members of
    /// an island. Going over faults the island
    /// ([`NodeFault::HostcallFuelExhausted`](crate::NodeFault::HostcallFuelExhausted)).
    /// Values passed *into* a guest are not charged. It bounds each lifted
    /// value, not the host's copies of it: the runtime keeps a Value packed
    /// (a list of numbers at its own size) and shared, and makes it into
    /// wasmtime's values again for each call that takes it.
    ///
    /// The default, 4 GiB (everything a 32-bit host addresses, on one),
    /// lets a `list<u8>` of about 85 MiB leave a node; lower it to bound
    /// what one call may make the host allocate.
    pub hostcall_fuel: usize,
    /// How much address space each linear memory reserves up front, rounded
    /// up to whole 64 KiB pages (`0` reserves nothing). `None` keeps
    /// wasmtime's default (4 GiB plus guards on 64-bit hosts): compiled
    /// code then skips bounds checks, but a process holds at most about 32k
    /// memories. A smaller reservation holds more memories at the cost of
    /// bounds-checked loads and stores; a memory that outgrows its
    /// reservation moves. Only used when the runtime makes the engine
    /// ([`engine`](Self::engine) is `None` and no prepared component brings
    /// one).
    pub memory_reservation: Option<u64>,
    /// The wasmtime engine to load components into: one from
    /// [`RuntimeConfig::new_engine`], or another graph's
    /// [`engine`](RuntimeGraph::engine), so several graphs share one. It is
    /// checked at load: it must meter fuel, support the component model's
    /// async ABI, value types and concurrency, have no epoch interruption,
    /// and no shared memories (which the memory limit cannot see). `None`
    /// uses the engine prepared components were compiled on, or else makes
    /// one. Either way, a loaded graph's [`config`](RuntimeGraph::config)
    /// holds the engine it runs on.
    pub engine: Option<Engine>,
    /// Whether members of an island joined only by streams of scalars or
    /// strings each get a Store of their own, the host pumping the items
    /// across ([islands](crate::engine#islands)). Split, those members
    /// interleave, a fault in a Store of one member names that member as
    /// its culprit (except a memory-limit fault: the island's Stores share
    /// the limit, and the Store that crossed it need not hold most of it),
    /// and the pumped items are traced
    /// ([`Trace::with_stream_items`](crate::Trace::with_stream_items)); but
    /// [`Host::island_data`] is made per Store (so capability state such as
    /// a plugin's [`Extensions`](crate::Extensions) is too), each Store has
    /// its own [`fuel_per_run`](Self::fuel_per_run) budget, and pumped items
    /// count against [`max_island_memory`](Self::max_island_memory). The
    /// default, `false`, keeps every island in one Store.
    pub split_islands: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps_per_tick: witgraph_sched::DEFAULT_MAX_STEPS_PER_TICK,
            yield_interval: 100_000,
            fuel_per_run: None,
            max_island_memory: Some(1 << 30),
            max_shared_memory: Some(usize::try_from(4u64 << 30).unwrap_or(usize::MAX)),
            // 4 GiB, or all of a 32-bit host's address space.
            hostcall_fuel: usize::try_from(4u64 << 30).unwrap_or(usize::MAX),
            memory_reservation: None,
            engine: None,
            split_islands: false,
        }
    }
}

impl RuntimeConfig {
    /// A wasmtime engine set up the way the runtime needs (the component
    /// model's async ABI and value types, fuel metering), with
    /// [`memory_reservation`](Self::memory_reservation).
    pub fn new_engine(&self) -> Result<Engine, RuntimeError> {
        engine::new_engine(self.memory_reservation).map_err(|e| RuntimeError::InvalidConfig {
            message: format!("{e:#}"),
        })
    }

    fn store_settings(&self) -> StoreSettings {
        StoreSettings {
            yield_interval: self.yield_interval,
            fuel_per_run: self.fuel_per_run,
            hostcall_fuel: self.hostcall_fuel,
        }
    }
}

/// A compiled graph loaded with WASM components, ready for execution.
///
/// `M` is the instrumentation mode; `H` is the embedder's [`Host`], which
/// provides capability imports and the data of every island Store.
pub struct RuntimeGraph<M: RuntimeMode = Perf, H: Host = NoCapabilities> {
    sched: Scheduler<M, Wasmtime<H>>,
    config: RuntimeConfig,
}

fn not_a_value_output(node: &NodeId, port: &PortName) -> RuntimeError {
    RuntimeError::NotAValuePort {
        node: node.clone(),
        port: port.clone(),
        direction: PortDirection::Output,
    }
}

/// Reads a [`RuntimeGraph`]'s latched outputs, also while it ticks.
#[derive(Clone)]
pub struct ValReader(LatchReader<Value>);

impl OutputReader for ValReader {
    type Value = Val;
    type Handle = Shared;

    async fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Val>, RuntimeError> {
        match self.0.read_output(node, port)? {
            Some(Value::Handle(_)) => Err(not_a_value_output(node, port)),
            value => Ok(value.map(|value| value.to_val())),
        }
    }

    async fn read_output_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        let Some(val) = self.read_output(node, port).await? else {
            return Ok(None);
        };
        val.to_wave()
            .map(Some)
            .map_err(|e| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message: e.to_string(),
            })
    }

    async fn read_handle(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Shared>, RuntimeError> {
        match self.0.read_output(node, port)? {
            Some(Value::Handle(shared)) => Ok(Some(shared)),
            None if self.0.output_kind(node, port)? == PortKind::Handle => Ok(None),
            _ => Err(not_a_value_output(node, port)),
        }
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.0.output_version(node, port)
    }
}

impl<M: RuntimeMode, H: Host> RunningGraph for RuntimeGraph<M, H> {
    type Value = Val;
    type Handle = Shared;
    type Reader = ValReader;

    fn reader(&self) -> ValReader {
        ValReader(self.sched.reader())
    }

    async fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        value: Val,
    ) -> Result<(), RuntimeError> {
        self.sched.inject(node, port, Value::from_val(value))
    }

    async fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        self.sched.inject_wave(node, port, text)
    }

    async fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.sched.clear_input(node, port)
    }

    async fn read_output(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Val>, RuntimeError> {
        self.reader().read_output(node, port).await
    }

    async fn read_output_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        self.reader().read_output_wave(node, port).await
    }

    async fn read_handle(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Shared>, RuntimeError> {
        self.reader().read_handle(node, port).await
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.sched.output_version(node, port)
    }

    fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.sched.rerun(node)
    }

    fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        self.sched.node_state(node)
    }

    fn take_faults(&mut self) -> Vec<FaultReport> {
        self.sched.take_faults()
    }

    fn shutdown(&mut self) {
        self.sched.shutdown();
    }

    async fn tick(&mut self) -> TickResult {
        self.sched.tick().await
    }

    async fn tick_until(&mut self, stop: impl Future<Output = ()> + Send) -> TickResult {
        self.sched.tick_until(stop).await
    }

    async fn snapshot(&mut self) -> Result<Snapshot, SnapshotError> {
        self.sched.snapshot()
    }

    async fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.sched.restore(snapshot)
    }

    fn compiled(&self) -> &CompiledGraph {
        self.sched.compiled()
    }
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// The memory the graph's shared objects take now, as their providers
    /// reported it: what counts against
    /// [`max_shared_memory`](RuntimeConfig::max_shared_memory).
    pub fn shared_memory(&self) -> usize {
        self.sched.executor().shared_memory.used()
    }

    /// The component type of a Value input port's payload (for an optional
    /// port, its inner type): what [`inject`](Self::inject) checks values
    /// against. Any Value input qualifies, connected or not.
    pub fn input_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        self.sched.port_type(node, port, PortDirection::Input)
    }

    /// The component type of a Value output port.
    pub fn output_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        self.sched.port_type(node, port, PortDirection::Output)
    }

    /// Cancels a node's island: an in-flight generation is dropped, with
    /// the island's Stores. See [`Scheduler::cancel`].
    pub fn cancel(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.sched.cancel(node)
    }

    /// The runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        self.sched.mode()
    }

    /// The wasmtime engine every island runs on; pass it as
    /// [`RuntimeConfig::engine`] to load another graph on it.
    pub fn engine(&self) -> &Engine {
        &self.sched.executor().engine
    }

    /// The embedder's host.
    pub fn host(&self) -> &H {
        &self.sched.executor().host
    }

    /// The embedder's host, to change capability state it shares with
    /// future island Stores.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.sched.executor_mut().host
    }
}
