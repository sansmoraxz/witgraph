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

mod compose;
pub(crate) mod load;

use std::future::Future;

use wasmtime::Engine;
use wasmtime::component::{Type, Val};
use witgraph_ir::{CompiledGraph, NodeId, PortDirection, PortName};
use witgraph_sched::{Scheduler, Snapshot};

pub use self::compose::LinkedProvider;
pub use self::load::PreparedComponent;
use crate::engine::{self, Host, NoCapabilities, StoreSettings, Wasmtime};
use witgraph_sched::{FaultReport, NodeState, Perf, RuntimeError, RuntimeMode, TickResult};

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
    /// Wasmtime's hostcall fuel, per call: roughly the bytes of host memory
    /// the values lifted *out of* a guest in one call may take, where every
    /// value element costs about 48 bytes. It bounds what a `run` returns,
    /// the arguments a guest passes to an imported function (a capability,
    /// or `fatal`), and stream and future items copied between members of
    /// an island. Going over faults the island
    /// ([`NodeFault::HostcallFuelExhausted`](crate::NodeFault::HostcallFuelExhausted)).
    /// Values passed *into* a guest are not charged. It bounds each lifted
    /// value, not the host's copies of it: a Value output is copied once
    /// for the host and once more for each in-island consumer, and every
    /// generation start copies its external inputs.
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
            hostcall_fuel: 128 << 20,
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

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Writes a value to a Value input port, as an external source would.
    /// See [`Scheduler::inject`] for which ports qualify and when the
    /// island re-runs.
    pub fn inject(&mut self, node: &NodeId, port: &PortName, val: Val) -> Result<(), RuntimeError> {
        self.sched.inject(node, port, val)
    }

    /// [`inject`](Self::inject)s a value written as WAVE text, parsed
    /// against the port's payload type. See [`Scheduler::inject_wave`].
    pub fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        self.sched.inject_wave(node, port, text)
    }

    /// Clears an injected Value input, as if nothing had been injected. See
    /// [`Scheduler::clear_input`].
    pub fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.sched.clear_input(node, port)
    }

    /// Makes the node's island owe a run on its latched inputs, as an input
    /// change would. See [`Scheduler::rerun`].
    pub fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.sched.rerun(node)
    }

    /// The latched value of a Value output port: what the node's `run`
    /// returned last, or `None` if it has not run yet. See
    /// [`Scheduler::read_output`].
    pub fn read_output(&self, node: &NodeId, port: &PortName) -> Result<Option<Val>, RuntimeError> {
        self.sched.read_output(node, port)
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

    /// A node's current lifecycle state: a view of its island's state at
    /// this moment (see [`NodeState`]). An unknown node is an error
    /// ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        self.sched.node_state(node)
    }

    /// Takes the faults reported since the last call. See
    /// [`Scheduler::take_faults`].
    pub fn take_faults(&mut self) -> Vec<FaultReport> {
        self.sched.take_faults()
    }

    /// Cancels a node's island: an in-flight generation is dropped, with
    /// the island's Stores. See [`Scheduler::cancel`].
    pub fn cancel(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.sched.cancel(node)
    }

    /// Stops everything: drops every in-flight generation and every Store.
    /// See [`Scheduler::shutdown`].
    pub fn shutdown(&mut self) {
        self.sched.shutdown();
    }

    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections. Dropping the future is safe: in-flight
    /// generations resume on the next tick. See [`Scheduler::tick`].
    pub async fn tick(&mut self) -> TickResult {
        self.sched.tick().await
    }

    /// Like [`tick`](Self::tick), but ends early when `stop` completes (a
    /// timer, say), with [`TickResult::Interrupted`]. See
    /// [`Scheduler::tick_until`].
    pub async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        self.sched.tick_until(stop).await
    }

    /// Captures the graph's host-visible state, at any time. Stream and
    /// future contents are not captured. See [`Scheduler::snapshot`] and
    /// [`Snapshot`].
    pub fn snapshot(&self) -> Snapshot {
        self.sched.snapshot()
    }

    /// Replaces the graph's host-visible state with `snapshot`; the next
    /// [`tick`](Self::tick) replays every generation that was in flight.
    /// See [`Scheduler::restore`] for the preconditions and what is
    /// restored.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.sched.restore(snapshot)
    }

    /// The compiled graph this runtime was loaded from.
    pub fn compiled(&self) -> &CompiledGraph {
        self.sched.compiled()
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
