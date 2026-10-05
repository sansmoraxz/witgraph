//! The `RuntimeGraph` typestate: a compiled graph loaded with WASM
//! components, ready for execution.
//!
//! Extends the typestate chain: `GraphBuilder -> Graph -> CompiledGraph ->
//! RuntimeGraph`.
//!
//! # Scheduling model
//!
//! The unit of execution is the island (see [`crate::engine`]): one Store
//! holding every node joined by stream/future connections. An island runs
//! in *generations*. A generation calls each member's `run` once, in
//! dependency order, and finishes when every call has returned and no
//! guest work is left in the Store.
//!
//! The host keeps one latched value per Value input port (written by
//! connections and [`inject`](RuntimeGraph::inject)) and one per Value
//! output port (latched when the node's `run` returns). That latched
//! state, pending feedback values, and the start inputs of every in-flight
//! or queued generation make up the host-visible state:
//! [`RuntimeGraph::snapshot`] captures it at any time and
//! [`RuntimeGraph::restore`] puts it back, replaying in-flight generations
//! from their start (stream/future contents and guest memory are never
//! captured; a deterministic guest regenerates them).
//!
//! Each island's lifecycle is a typestate (crate-private `island` module):
//! `Idle` (owns the Store), `Running` (owns the generation future, which
//! owns the Store), and `Stopped` (faulted, cancelled, shut down or
//! restored; no Store). A node's [`NodePhase`] is a projection of its
//! island's state. What an island still has to run is kept apart from its
//! phase, as a queue of *owed* generations: a run on the latched inputs, or
//! a replay of a restored generation. Invariants:
//!
//! - A newly loaded island owes one run. An external Value input that
//!   changes (`Val ==` decides: `0.0` and `-0.0` differ, every NaN equals
//!   every other) makes its island owe a run, whatever phase the island is
//!   in. Owed runs on the latched inputs coalesce: any number of changes
//!   before the next start owe one run.
//! - An island starts its next owed generation only when it is not running;
//!   for a run on the latched inputs, every required external Value input of
//!   every member has a value (a replay brings its own inputs); its resource
//!   claims fit; and nothing *upstream* of it can still change its inputs
//!   (computed by `unsettled` in `run.rs`). That keeps a node from running on
//!   half-updated inputs, while a Value latched mid-generation (by a
//!   streaming island that never finishes, say) still wakes everything
//!   downstream of it. Compilation merges islands that a non-feedback path
//!   leaves and re-enters, so the islands form a DAG and this wait never
//!   closes a cycle. Islands are visited in the compiled graph's island
//!   order, which is topological. A stopped island's generation first
//!   rebuilds its Store, inside the generation's future.
//! - A generation in flight is never pre-empted: inputs that change while
//!   it runs are owed, and the island re-runs (with fresh stream/future
//!   handles for every member) after it finishes.
//! - A Value output reaching another island is delivered as soon as the
//!   producing `run` returns. A Value output reaching a member of the same
//!   island over a non-feedback connection is part of the generation and
//!   owes nothing.
//! - Feedback connections (Value only) are latched once per tick, at
//!   quiescence: that is one iteration of a loop. A tick that ends early
//!   (`StepLimitReached`, `Aborted`) latches nothing; a later tick finishes
//!   the iteration. A dropped or interrupted tick latches, as it ends, the
//!   feedback into every island whose iteration is over: the island has
//!   settled, and so has every feedback source into it.
//! - A host write that changes an input wins over older feedback: it drops
//!   feedback buffered for that input, and marks the feedback connections
//!   into it stale for the generation in flight at their source (or the
//!   replay their source owes), which then never delivers them.
//! - A fault (trap, fuel exhaustion, a limit, `fatal`) stops the island and
//!   faults every node of it, recording the culprit when it is known and
//!   the fault for [`take_faults`](RuntimeGraph::take_faults). Owed work
//!   survives a fault, so an input that changed while the failing
//!   generation ran makes the island rebuild and run again; otherwise a
//!   faulted island waits for its next input change or
//!   [`rerun`](RuntimeGraph::rerun). A rebuild that fails faults every
//!   member with [`NodeFault::Restart`](crate::NodeFault::Restart) and drops the generation it was
//!   starting; work owed since then is still owed.
//! - Cancellation and shutdown drop the generation and the Store and forget
//!   owed work, including feedback values buffered for the island; the
//!   island is rebuilt on its next input change or `rerun`. A restore drops
//!   every island's Store, so no guest state outlives it.
//!
//! In-flight generations live in the runtime graph, not in the future
//! returned by [`tick`](RuntimeGraph::tick): dropping a tick (on a
//! timeout, say) loses nothing, and the next tick resumes them. Every
//! `&mut self` entry point first handles the events in-flight generations
//! sent since the last tick, so a `run` that returned before a dropped
//! tick, a cancel or a shutdown still has its outputs latched and
//! delivered.

mod load;
mod run;
mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::task::Waker;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use wasmtime::Engine;
use wasmtime::component::{Type, Val};
use witgraph_ir::{
    CompiledGraph, ComponentRef, Connection, ConnectionId, NodeId, PortDirection, PortName, PortRef,
};

pub use self::load::PreparedComponent;
use self::run::WakeSet;
use crate::engine::{
    self, Host, IslandEvent, IslandPlan, NoCapabilities, NodeBinary, StoreSettings,
};
use crate::error::RuntimeError;
use crate::island::{IslandState, Owed, StopCause};
use crate::mode::{Perf, RuntimeMode};
use crate::node::{NodePhase, NodeState};
use crate::resource::ResourcePool;
use crate::schedule::FaultReport;

/// WAVE text per port, keyed by node id then port name.
pub type PortValues = BTreeMap<NodeId, BTreeMap<PortName, String>>;

/// The host-visible state of a runtime graph at one point in time, for
/// point-in-time restores and debugging. Produced by
/// [`RuntimeGraph::snapshot`] (at any time), applied by
/// [`RuntimeGraph::restore`].
///
/// Values are WAVE text, parsed against the port types on restore. Only
/// what the host sees is captured: latched Values, pending feedback, node
/// phases, and the inputs each in-flight or queued generation starts with.
/// Guest memory (including suspended tasks) cannot be read out of wasmtime
/// and is not captured either.
///
/// Snapshots do not store stream or future contents. Restoring re-runs any
/// generation that was in flight, which recreates its streams from the
/// recorded inputs; the result matches the original only if the guests are
/// deterministic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Each node's resolved component (id and content hash). A snapshot
    /// restores only onto a graph with exactly these nodes and components.
    pub nodes: BTreeMap<NodeId, ComponentRef>,
    /// The graph's connections, sorted by id. A snapshot restores only onto
    /// a graph wired exactly the same way.
    pub connections: Vec<Connection>,
    /// Whether no generation was in flight when the snapshot was taken. A
    /// replay owed after a restore is not in flight, so it is listed in
    /// [`islands`](Self::islands) while this is `true`.
    pub quiescent: bool,
    /// Every node's phase when the snapshot was taken. Informational:
    /// [`RuntimeGraph::restore`] does not re-fault or re-cancel islands.
    pub phases: BTreeMap<NodeId, NodePhase>,
    /// Latched Value inputs: injected, delivered by connections, or latched
    /// from feedback.
    pub inputs: PortValues,
    /// Latched Value outputs: what each node's `run` returned last
    /// (including `run`s of an in-flight generation that already returned).
    pub outputs: PortValues,
    /// Feedback values waiting for the next latch, by connection, as the
    /// source port produced them.
    pub feedback: BTreeMap<ConnectionId, String>,
    /// Islands with an in-flight or queued generation.
    pub islands: Vec<IslandSnapshot>,
}

/// An island with work outstanding when a [`Snapshot`] was taken.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IslandSnapshot {
    /// The island's members, sorted. [`RuntimeGraph::restore`] matches
    /// islands by their set of members, in any order.
    pub members: Vec<NodeId>,
    /// The external Value inputs the in-flight generation started with, by
    /// member, or those of a restored replay not started yet; `None` when
    /// there is neither.
    pub running: Option<PortValues>,
    /// Whether another generation was queued (an input changed since the
    /// last start). It runs with the latched inputs.
    pub queued: bool,
    /// The feedback connections out of this island whose target the host
    /// wrote after the [`running`](Self::running) generation started: what
    /// that generation feeds back there is stale, and its replay drops it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub stale_feedback: BTreeSet<ConnectionId>,
}

/// Configuration for a runtime graph.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Most generations one tick may start before it returns
    /// [`TickResult::StepLimitReached`](crate::TickResult::StepLimitReached): a bound on how long one tick can
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
    /// streaming generation exhausts any finite budget eventually. `None`
    /// means effectively unlimited, instantiation included.
    pub fuel_per_run: Option<u64>,
    /// Bytes of linear memory and tables (8 bytes per element) an island's
    /// instances may hold in total. Growing past it faults the island
    /// ([`NodeFault::MemoryLimit`](crate::NodeFault::MemoryLimit)). `None`
    /// is unlimited. An island may also hold at most 16 memories, 16
    /// tables and 64 core instances per member. Host-side copies of values
    /// are not counted (see [`hostcall_fuel`](Self::hostcall_fuel)).
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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps_per_tick: 10_000,
            yield_interval: 100_000,
            fuel_per_run: None,
            max_island_memory: Some(1 << 30),
            hostcall_fuel: 128 << 20,
            memory_reservation: None,
            engine: None,
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

/// An outgoing connection of a Value output port.
#[derive(Debug, Clone)]
struct Edge {
    id: ConnectionId,
    to: PortRef,
    route: Route,
    /// An `option<T>` output feeding an optional input of payload `T`
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    unwrap_option: bool,
}

/// How a delivered Value travels along an [`Edge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Buffered until the end-of-iteration latch.
    Feedback,
    /// To a member of the producer's own island: part of the generation.
    Internal,
    /// To another island, at once.
    External,
}

/// A buffered feedback write: the target, whether it unwraps an option,
/// and the value as the source produced it.
type FeedbackWrite = (PortRef, bool, Arc<Val>);

/// One island: its static wiring, its lifecycle, and the work it owes.
struct Slot<D: 'static> {
    plan: Arc<IslandPlan>,
    /// What instantiating each member needs, in plan order.
    binaries: Vec<Arc<NodeBinary<D>>>,
    /// Where the island's members start in the graph-wide node index (the
    /// members are `offset..offset + members.len()`).
    offset: usize,
    /// Per member, in plan order: the nodes (graph-wide index) that write
    /// one of its inputs over a non-feedback connection.
    preds: Vec<Vec<usize>>,
    /// The nodes (graph-wide index) that write one of the island's inputs
    /// over a feedback connection.
    feedback_sources: Vec<usize>,
    /// The feedback connections out of the island's members.
    feedback_out: Vec<ConnectionId>,
    /// Every member's required external Value inputs: a run on the latched
    /// inputs needs a value for each.
    required: Vec<PortRef>,
    /// Per member, in plan order: whether its `run` is still to return in
    /// the generation in flight. All `false` when nothing is in flight.
    awaiting: Vec<bool>,
    /// Feedback connections the generation in flight must not deliver:
    /// the host wrote their target since it started (or, for a replay,
    /// since the original generation started).
    stale_feedback: BTreeSet<ConnectionId>,
    /// Wakes the tick for this island's generation alone.
    waker: Waker,
    state: IslandState<D>,
    owed: Owed,
}

/// A compiled graph loaded with WASM components, ready for execution.
///
/// `M` is the instrumentation mode; `H` is the embedder's [`Host`], which
/// provides capability imports and the data of every island Store.
pub struct RuntimeGraph<M: RuntimeMode = Perf, H: Host = NoCapabilities> {
    compiled: CompiledGraph,
    config: RuntimeConfig,
    mode: M,
    host: H,
    engine: Engine,
    settings: StoreSettings,
    /// Islands, in the compiled graph's (topological) island order.
    slots: Vec<Slot<H::Data>>,
    /// How many islands have a generation in flight.
    running: usize,
    /// Each island's index, by its sorted members: how a snapshot names it.
    island_index: HashMap<Vec<NodeId>, usize>,
    /// Latched value of every Value input port that has one.
    inputs: HashMap<PortRef, Arc<Val>>,
    /// Payload type of every Value input port (inner type when optional).
    input_types: HashMap<PortRef, Type>,
    /// Type of every Value output port.
    output_types: HashMap<PortRef, Type>,
    /// Latched Value outputs.
    outputs: HashMap<PortRef, Arc<Val>>,
    out_edges: HashMap<PortRef, Arc<[Edge]>>,
    /// Every feedback connection, by id, with its source port.
    feedback_edges: HashMap<ConnectionId, (PortRef, Edge)>,
    /// The feedback connections into each input port, with the island of
    /// their source.
    feedback_into: HashMap<PortRef, Vec<(ConnectionId, usize)>>,
    /// The graph's connections, sorted by id: how snapshots name the
    /// wiring.
    connections: Vec<Connection>,
    /// Each node's resolved component: how snapshots name the nodes.
    components: BTreeMap<NodeId, ComponentRef>,
    /// Input ports written by a non-feedback connection, with that
    /// connection: the one writer [`inject`](Self::inject) may not compete
    /// with.
    connected: HashMap<PortRef, ConnectionId>,
    /// Feedback writes waiting for the end-of-iteration latch.
    feedback: BTreeMap<ConnectionId, FeedbackWrite>,
    /// The latest fault of each island not yet taken by
    /// [`take_faults`](Self::take_faults).
    faults: BTreeMap<usize, FaultReport>,
    /// Which running islands to poll.
    wakes: Arc<WakeSet>,
    /// Whether anything changed since the last start pass that could let
    /// an island start.
    dirty: bool,
    events_tx: UnboundedSender<IslandEvent>,
    events_rx: UnboundedReceiver<IslandEvent>,
    resources: ResourcePool,
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Writes a value to a Value input port, as an external source would.
    ///
    /// The port must be unconnected or fed only by feedback connections:
    /// an input has one writer, so one a connection writes is rejected
    /// ([`RuntimeError::ConnectedInput`]). The value must have the port's
    /// payload type (for an optional port, the inner type). Writing a value
    /// equal to the current one is not a change and does nothing;
    /// otherwise the node's island re-runs on the next tick (a faulted or
    /// cancelled island is rebuilt first), and the write wins over any
    /// feedback for this input computed before it.
    pub fn inject(&mut self, node: &NodeId, port: &PortName, val: Val) -> Result<(), RuntimeError> {
        self.drain_events();
        let (port_ref, ty) = self.host_input(node, port)?;
        engine::check_type(&ty, &val).map_err(|message| RuntimeError::ValueType {
            node: node.clone(),
            port: port.clone(),
            message,
        })?;
        let val = engine::canonical(&ty, val);
        if self.set_input(port_ref.clone(), Arc::new(val)) {
            self.note_host_write(&port_ref);
        }
        Ok(())
    }

    /// [`inject`](Self::inject)s a value written as WAVE text, parsed
    /// against the port's payload type ([`RuntimeError::ValueType`] if it
    /// does not parse). Record fields, cases and flags the type does not
    /// have are rejected.
    pub fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        let (_, ty) = self.host_input(node, port)?;
        let val = engine::parse_wave(&ty, text).map_err(|message| RuntimeError::ValueType {
            node: node.clone(),
            port: port.clone(),
            message,
        })?;
        self.inject(node, port, val)
    }

    /// Clears an injected Value input, as if nothing had been injected: an
    /// optional input reads as `none` again, and a required one leaves its
    /// island waiting for a value. The same ports as
    /// [`inject`](Self::inject) qualify. Clearing an input that has no value
    /// is not a change and does nothing; otherwise the island re-runs, and
    /// the clear wins over feedback for this input computed before it.
    pub fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.drain_events();
        let (port_ref, _) = self.host_input(node, port)?;
        if self.clear_input_value(port_ref.clone()) {
            self.note_host_write(&port_ref);
        }
        Ok(())
    }

    /// Makes the node's island owe a run on its latched inputs, as an input
    /// change would; a stopped island is rebuilt first. This is how an
    /// island whose inputs the host cannot change (a source driven by a
    /// capability, say) runs again after a fault, cancel or shutdown.
    pub fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.drain_events();
        let index = self.island(node)?;
        self.slots[index].owed.push_latched();
        self.dirty = true;
        Ok(())
    }

    /// The latched value of a Value output port: what the node's `run`
    /// returned last, or `None` if it has not run yet. An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is not a Value output
    /// ([`RuntimeError::NotAValuePort`]) is an error.
    pub fn read_output(&self, node: &NodeId, port: &PortName) -> Result<Option<Val>, RuntimeError> {
        let port_ref = PortRef::new(node.clone(), port.clone());
        self.output_type(node, port)?;
        Ok(self.outputs.get(&port_ref).map(|val| (**val).clone()))
    }

    /// The component type of a Value input port's payload (for an optional
    /// port, its inner type): what [`inject`](Self::inject) checks values
    /// against. Any Value input qualifies, connected or not.
    pub fn input_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        self.island(node)?;
        self.input_types
            .get(&PortRef::new(node.clone(), port.clone()))
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Input,
            })
    }

    /// The component type of a Value output port.
    pub fn output_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        self.island(node)?;
        self.output_types
            .get(&PortRef::new(node.clone(), port.clone()))
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Output,
            })
    }

    /// A node's current lifecycle state: a view of its island's state at
    /// this moment (see [`crate::node`]). An unknown node is an error
    /// ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let index = self.island(node)?;
        let state = &self.slots[index].state;
        let contract = load::contract_of(&self.compiled, node)?;
        Ok(NodeState::new(
            node.clone(),
            contract.shape(),
            state.node_phase(),
            state.fault().cloned(),
            state.culprit().cloned(),
        ))
    }

    /// Takes the faults reported since the last call: the latest fault of
    /// each island that faulted, in island order. A tick ends early only
    /// for `fatal` ([`TickResult::Aborted`](crate::TickResult::Aborted)); every other fault (a trap,
    /// exhausted fuel, a limit, a failed rebuild) is reported here, and in
    /// [`node_state`](Self::node_state) while the island stays faulted.
    pub fn take_faults(&mut self) -> Vec<FaultReport> {
        self.drain_events();
        std::mem::take(&mut self.faults).into_values().collect()
    }

    /// Cancels a node's island: an in-flight generation is dropped (and
    /// with it the island's Store), any work the island owed is forgotten
    /// (feedback values buffered for it included), and every node of the
    /// island becomes [`Cancelled`](NodePhase::Cancelled). A faulted island
    /// becomes cancelled too. Cancelling an island that is already
    /// cancelled forgets the work it was owed since, and reports nothing.
    /// The island is rebuilt when an input next changes, or on
    /// [`rerun`](Self::rerun). Islands it was holding back may start on the
    /// next tick.
    pub fn cancel(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        let index = self.island(node)?;
        self.drain_events();
        self.stop_island(index, StopCause::Cancelled);
        Ok(())
    }

    /// Stops everything: drops every in-flight generation and every Store,
    /// forgets all owed work and every buffered feedback value, and cancels
    /// every island that is still live. A faulted island keeps its fault. A
    /// later input change rebuilds the affected island as after
    /// [`cancel`](Self::cancel).
    pub fn shutdown(&mut self) {
        self.drain_events();
        for index in 0..self.slots.len() {
            self.stop_island(index, StopCause::Shutdown);
        }
        self.feedback.clear();
    }

    /// The compiled graph this runtime was loaded from.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// The runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// The wasmtime engine every island runs on; pass it as
    /// [`RuntimeConfig::engine`] to load another graph on it.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The embedder's host.
    pub fn host(&self) -> &H {
        &self.host
    }

    /// The embedder's host, to change capability state it shares with
    /// future island Stores.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// The node's island index.
    fn island(&self, node: &NodeId) -> Result<usize, RuntimeError> {
        self.compiled
            .island_of(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
    }

    /// A Value input the host may write ([`inject`](Self::inject),
    /// [`clear_input`](Self::clear_input)), with its payload type.
    fn host_input(&self, node: &NodeId, port: &PortName) -> Result<(PortRef, Type), RuntimeError> {
        self.island(node)?;
        let port_ref = PortRef::new(node.clone(), port.clone());
        let Some(ty) = self.input_types.get(&port_ref) else {
            return Err(RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Input,
            });
        };
        if let Some(connection) = self.connected.get(&port_ref) {
            return Err(RuntimeError::ConnectedInput {
                node: node.clone(),
                port: port.clone(),
                connection: connection.clone(),
            });
        }
        Ok((port_ref, ty.clone()))
    }
}

/// Dropping a graph drops every generation in flight; each is reported
/// stopped, so every `GenerationStarted` a mode saw has its end.
impl<M: RuntimeMode, H: Host> Drop for RuntimeGraph<M, H> {
    fn drop(&mut self) {
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(generation) = slot.state.running_generation() {
                self.mode.on_generation_stopped(index, generation);
            }
        }
    }
}
