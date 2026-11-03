//! The scheduler: a compiled graph's islands, their latched values and
//! the work they owe, driven over an [`Executor`].
//!
//! # Scheduling model
//!
//! The unit of execution is the island: every node joined by stream/future
//! connections, sharing one live island of the [`Executor`] (on wasmtime, a
//! Store). An island runs
//! in *generations*. A generation calls each member's `run` once, in
//! dependency order, and finishes when every call has returned and no
//! guest work is left in the Store.
//!
//! The host keeps one latched value per Value input port (written by
//! connections and [`inject`](Scheduler::inject)) and one per Value
//! output port (latched when the node's `run` returns). That latched
//! state, pending feedback values, and the start inputs of every in-flight
//! or queued generation make up the host-visible state:
//! [`Scheduler::snapshot`] captures it at any time and
//! [`Scheduler::restore`] puts it back, replaying in-flight generations
//! from their start (stream/future contents and guest memory are never
//! captured; a deterministic guest regenerates them).
//!
//! Each island's lifecycle is a typestate (crate-private `island` module):
//! `Idle` (owns the Store), `Running` (owns the generation future, which
//! owns the Store), and `Stopped` (faulted, cancelled, shut down or
//! restored; no Store). A node's [`NodePhase`](crate::NodePhase) is a projection of its
//! island's state. What an island still has to run is kept apart from its
//! phase, as a queue of *owed* generations: a run on the latched inputs, or
//! a replay of a restored generation. Invariants:
//!
//! - A newly loaded island owes one run. An external Value input that
//!   changes (value equality decides, as the executor's value type
//!   defines it) makes its island owe a run, whatever phase the island is
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
//!   the fault for [`take_faults`](Scheduler::take_faults). Owed work
//!   survives a fault, so an input that changed while the failing
//!   generation ran makes the island rebuild and run again; otherwise a
//!   faulted island waits for its next input change or
//!   [`rerun`](Scheduler::rerun). A rebuild that fails faults every
//!   member with [`NodeFault::Restart`](crate::NodeFault::Restart) and drops the generation it was
//!   starting; work owed since then is still owed.
//! - Cancellation and shutdown drop the generation and the Store and forget
//!   owed work, including feedback values buffered for the island; the
//!   island is rebuilt on its next input change or `rerun`. A restore drops
//!   every island's Store, so no guest state outlives it.
//!
//! In-flight generations live in the scheduler, not in the future
//! returned by [`tick`](Scheduler::tick): dropping a tick (on a
//! timeout, say) loses nothing, and the next tick resumes them. Every
//! `&mut self` entry point first handles the events in-flight generations
//! sent since the last tick, so a `run` that returned before a dropped
//! tick, a cancel or a shutdown still has its outputs latched and
//! delivered.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::task::Waker;

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use witgraph_ir::{
    CompiledGraph, ComponentRef, Connection, ConnectionId, FeedbackIdx, Idx, IndexVec, InputIdx,
    Link, NodeId, NodeIdx, OutputIdx, PortDirection, PortName, PortSet, PortTable, ResourceId,
};

use crate::error::RuntimeError;
use crate::executor::{Canonical, Executor, IslandEvent};
use crate::island::{Island, IslandState, Owed, StopCause};
use crate::mode::RuntimeMode;
use crate::node::NodeState;
use crate::plan::{self, IslandPlan, RunShape, Wiring};
use crate::resource::ResourcePool;
use crate::run::{IslandWaker, WakeSet};
use crate::schedule::FaultReport;

/// The latched value of each port or connection of one kind that has one.
pub(crate) type Latches<I, V> = IndexVec<I, Option<Arc<V>>>;

/// An outgoing connection of a Value output port.
#[derive(Debug, Clone)]
pub(crate) struct Edge {
    pub(crate) to: InputIdx,
    pub(crate) route: Route,
    /// An `option<T>` output feeding an optional input of payload `T`
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    pub(crate) unwrap_option: bool,
}

/// How a delivered Value travels along an [`Edge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Buffered until the end-of-iteration latch.
    Feedback(FeedbackIdx),
    /// To a member of the producer's own island: part of the generation.
    Internal,
    /// To another island, at once.
    External,
}

/// One island: its static wiring, its lifecycle, and the work it owes.
pub(crate) struct Slot<X: Executor> {
    pub(crate) plan: Arc<IslandPlan>,
    /// Where the island's members start in the graph-wide node index (the
    /// members are `offset..offset + members.len()`).
    pub(crate) offset: NodeIdx,
    /// Per member, in plan order: the nodes that write one of its inputs
    /// over a non-feedback connection.
    pub(crate) preds: Vec<Vec<NodeIdx>>,
    /// The nodes that write one of the island's inputs over a feedback
    /// connection.
    pub(crate) feedback_sources: Vec<NodeIdx>,
    /// The feedback connections out of the island's members.
    pub(crate) feedback_out: Vec<FeedbackIdx>,
    /// Every member's required external Value inputs: a run on the latched
    /// inputs needs a value for each.
    pub(crate) required: Vec<InputIdx>,
    /// Per member, in plan order: whether its `run` is still to return in
    /// the generation in flight. All `false` when nothing is in flight.
    pub(crate) awaiting: Vec<bool>,
    /// Feedback connections the generation in flight must not deliver:
    /// the host wrote their target since it started (or, for a replay,
    /// since the original generation started).
    pub(crate) stale_feedback: PortSet<FeedbackIdx>,
    /// Wakes the tick for this island's generation alone.
    pub(crate) waker: Waker,
    pub(crate) state: IslandState<X::Island, X::Value>,
    pub(crate) owed: Owed<X::Value>,
}

/// A compiled graph being run: the last stage of the typestate chain
/// `GraphBuilder -> Graph -> CompiledGraph`, over an [`Executor`].
///
/// `M` is the instrumentation mode; `X` runs the nodes.
pub struct Scheduler<M: RuntimeMode, X: Executor> {
    pub(crate) compiled: CompiledGraph,
    pub(crate) mode: M,
    pub(crate) executor: X,
    /// Most generations one tick may start.
    pub(crate) max_steps_per_tick: usize,
    /// Islands, in the compiled graph's (topological) island order.
    pub(crate) slots: Vec<Slot<X>>,
    /// How many islands have a generation in flight.
    pub(crate) running: usize,
    /// Each island's index, by its sorted members: how a snapshot names it.
    pub(crate) island_index: HashMap<Vec<NodeId>, usize>,
    /// Every port and feedback connection, numbered.
    pub(crate) ports: PortTable,
    /// Each node's island.
    pub(crate) island_of_node: IndexVec<NodeIdx, usize>,
    /// The latched value of each input port. Only Value and handle ports
    /// ever hold one.
    pub(crate) inputs: Latches<InputIdx, X::Value>,
    /// The latched value of each output port, likewise.
    pub(crate) outputs: Latches<OutputIdx, X::Value>,
    pub(crate) out_edges: IndexVec<OutputIdx, Arc<[Edge]>>,
    /// The feedback connections into each input port, with the island of
    /// their source.
    pub(crate) feedback_into: IndexVec<InputIdx, Box<[(FeedbackIdx, usize)]>>,
    /// The graph's connections, sorted by id: how snapshots name the
    /// wiring.
    pub(crate) connections: Vec<Connection>,
    /// Each node's resolved component: how snapshots name the nodes.
    pub(crate) components: BTreeMap<NodeId, ComponentRef>,
    /// The graph's links, sorted by id: how snapshots name what is
    /// composed into the nodes.
    pub(crate) links: Vec<Link>,
    /// The non-feedback connection writing each input port, if one does:
    /// the one writer [`inject`](Self::inject) may not compete with.
    pub(crate) connected: IndexVec<InputIdx, Option<ConnectionId>>,
    /// Feedback writes waiting for the end-of-iteration latch: the value
    /// as the source produced it, by connection.
    pub(crate) feedback: Latches<FeedbackIdx, X::Value>,
    /// The latest fault of each island not yet taken by
    /// [`take_faults`](Self::take_faults).
    pub(crate) faults: BTreeMap<usize, FaultReport>,
    /// Which running islands to poll.
    pub(crate) wakes: Arc<WakeSet>,
    /// Whether anything changed since the last start pass that could let
    /// an island start.
    pub(crate) dirty: bool,
    pub(crate) events_tx: UnboundedSender<IslandEvent<X::Value>>,
    pub(crate) events_rx: UnboundedReceiver<IslandEvent<X::Value>>,
    pub(crate) resources: ResourcePool,
}

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
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
    pub fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        val: X::Value,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.host_input(node, port)?;
        let val =
            self.executor
                .check_input(ty, val)
                .map_err(|message| RuntimeError::ValueType {
                    node: node.clone(),
                    port: port.clone(),
                    message,
                })?;
        self.inject_checked(input, val);
        Ok(())
    }

    /// Writes a value already in the port's checked, canonical form.
    fn inject_checked(&mut self, input: InputIdx, val: Canonical<X::Value>) {
        self.drain_events();
        if self.set_input(input, Arc::new(val.into_inner())) {
            self.note_host_write(input);
        }
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
        let (input, ty) = self.host_input(node, port)?;
        let val =
            self.executor
                .parse_wave(ty, text)
                .map_err(|message| RuntimeError::ValueType {
                    node: node.clone(),
                    port: port.clone(),
                    message,
                })?;
        self.inject_checked(input, val);
        Ok(())
    }

    /// Clears an injected Value input, as if nothing had been injected: an
    /// optional input reads as `none` again, and a required one leaves its
    /// island waiting for a value. The same ports as
    /// [`inject`](Self::inject) qualify. Clearing an input that has no value
    /// is not a change and does nothing; otherwise the island re-runs, and
    /// the clear wins over feedback for this input computed before it.
    pub fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.drain_events();
        let (input, _) = self.host_input(node, port)?;
        if self.clear_input_value(input) {
            self.note_host_write(input);
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

    /// The latched value of a Value or handle output port: what the node's
    /// `run` returned last, or `None` if it has not run yet. An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is neither
    /// ([`RuntimeError::NotAValuePort`]) is an error.
    pub fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<X::Value>, RuntimeError> {
        let output = self.latched_output(node, port)?;
        Ok(self.outputs[output].as_ref().map(|val| (**val).clone()))
    }

    /// The type of a Value port: for an input, its payload type (for an
    /// optional port, the inner type). An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is not a Value port of
    /// that direction ([`RuntimeError::NotAValuePort`]) is an error.
    pub fn port_type(
        &self,
        node: &NodeId,
        port: &PortName,
        direction: PortDirection,
    ) -> Result<&X::Type, RuntimeError> {
        self.value_port(node, port, direction)
    }

    /// The payload type of a Value input the host may write, as
    /// [`inject`](Self::inject) checks it: an error for an input a
    /// connection writes ([`RuntimeError::ConnectedInput`]) as for one that
    /// is no Value input. For an executor that converts a value before
    /// injecting it.
    pub fn host_input_type(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<&X::Type, RuntimeError> {
        self.host_input(node, port).map(|(_, ty)| ty)
    }

    /// Checks that `node` exists and `port` is a Value or handle output of
    /// it: one whose values the host latches.
    fn latched_output(&self, node: &NodeId, port: &PortName) -> Result<OutputIdx, RuntimeError> {
        self.island(node)?;
        self.ports
            .output(node, port)
            .filter(|output| self.ports.outputs()[*output].kind.is_latched())
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Output,
            })
    }

    /// Checks that `node` exists and `port` is a Value port of it in
    /// `direction`, and finds its type.
    fn value_port(
        &self,
        node: &NodeId,
        port: &PortName,
        direction: PortDirection,
    ) -> Result<&X::Type, RuntimeError> {
        self.island(node)?;
        let entry = match direction {
            PortDirection::Input => self
                .ports
                .input(node, port)
                .map(|input| &self.ports.inputs()[input]),
            PortDirection::Output => self
                .ports
                .output(node, port)
                .map(|output| &self.ports.outputs()[output]),
        };
        entry
            .and_then(|entry| self.executor.port_type(&entry.port, direction))
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction,
            })
    }

    /// A node's current lifecycle state: a view of its island's state at
    /// this moment (see [`crate::node`]). An unknown node is an error
    /// ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let index = self.island(node)?;
        let state = &self.slots[index].state;
        let contract = self
            .compiled
            .contract_for(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
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
    /// island becomes [`Cancelled`](crate::NodePhase::Cancelled). A faulted island
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
        self.feedback = IndexVec::from_fn(self.feedback.len(), |_| None);
    }

    /// The compiled graph being run.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// What runs the nodes.
    pub fn executor(&self) -> &X {
        &self.executor
    }

    /// What runs the nodes, to change state it shares with islands it
    /// builds later.
    pub fn executor_mut(&mut self) -> &mut X {
        &mut self.executor
    }

    /// The most generations one tick may start before it returns
    /// [`TickResult::StepLimitReached`](crate::TickResult::StepLimitReached).
    pub fn max_steps_per_tick(&self) -> usize {
        self.max_steps_per_tick
    }

    /// Sets [`max_steps_per_tick`](Self::max_steps_per_tick); at least 1
    /// ([`RuntimeError::InvalidConfig`]).
    pub fn set_max_steps_per_tick(&mut self, steps: usize) -> Result<(), RuntimeError> {
        check_max_steps_per_tick(steps)?;
        self.max_steps_per_tick = steps;
        Ok(())
    }

    /// The node's island index.
    fn island(&self, node: &NodeId) -> Result<usize, RuntimeError> {
        self.compiled
            .island_of(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
    }

    /// A Value input the host may write ([`inject`](Self::inject),
    /// [`clear_input`](Self::clear_input)).
    fn host_input(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(InputIdx, &X::Type), RuntimeError> {
        let ty = self.value_port(node, port, PortDirection::Input)?;
        let not_a_port = || RuntimeError::NotAValuePort {
            node: node.clone(),
            port: port.clone(),
            direction: PortDirection::Input,
        };
        let input = self.ports.input(node, port).ok_or_else(not_a_port)?;
        if let Some(connection) = &self.connected[input] {
            return Err(RuntimeError::ConnectedInput {
                node: node.clone(),
                port: port.clone(),
                connection: connection.clone(),
            });
        }
        Ok((input, ty))
    }
}

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
    /// A scheduler over a compiled graph whose islands `executor` has
    /// built.
    ///
    /// `islands` holds every island's live island and the shape of each
    /// member's `run`, both in the compiled graph's island order (and, for
    /// the shapes, in the island's member order). Every island owes its
    /// first generation. `max_steps_per_tick` is at least 1
    /// ([`RuntimeError::InvalidConfig`]), as must be the claims of every
    /// island on each resource fit in it.
    pub fn new(
        compiled: CompiledGraph,
        mode: M,
        executor: X,
        islands: Vec<(X::Island, Vec<RunShape>)>,
        max_steps_per_tick: usize,
    ) -> Result<Self, RuntimeError> {
        check_max_steps_per_tick(max_steps_per_tick)?;
        if islands.len() != compiled.islands().len() {
            return Err(RuntimeError::InvalidConfig {
                message: format!(
                    "the executor built {} islands; the graph has {}",
                    islands.len(),
                    compiled.islands().len()
                ),
            });
        }
        let resources = register_resources(&compiled)?;
        let graph = compiled.graph();
        let wiring = Wiring::new(&compiled);
        let ports = PortTable::new(&compiled);
        let island_of_node: IndexVec<NodeIdx, usize> = compiled
            .islands()
            .iter()
            .enumerate()
            .flat_map(|(island, members)| members.iter().map(move |_| island))
            .collect();
        let mut out_edges: Vec<Vec<Edge>> = vec![Vec::new(); ports.outputs().len()];
        let mut feedback_into: Vec<Vec<(FeedbackIdx, usize)>> =
            vec![Vec::new(); ports.inputs().len()];
        let island_of: Vec<usize> = compiled.nodes().map(|n| n.island).collect();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (from, to) = (island_of[resolved.from_node], island_of[resolved.to_node]);
            let (Some(output), Some(input)) = (
                ports.output(&conn.from.node, &conn.from.port),
                ports.input(&conn.to.node, &conn.to.port),
            ) else {
                continue;
            };
            let route = match (ports.feedback_index(&conn.id), from == to) {
                (Some(feedback), _) => {
                    feedback_into[input.position()].push((feedback, from));
                    Route::Feedback(feedback)
                }
                (None, true) => Route::Internal,
                (None, false) => Route::External,
            };
            out_edges[output.position()].push(Edge {
                to: input,
                route,
                unwrap_option: resolved.unwraps_option,
            });
        }
        let out_edges = out_edges.into_iter().map(Arc::from).collect();
        let feedback_into = feedback_into
            .into_iter()
            .map(Vec::into_boxed_slice)
            .collect();
        let mut connections = graph.connections.clone();
        connections.sort_by(|a, b| a.id.cmp(&b.id));
        let mut links = graph.links.clone();
        links.sort_by(|a, b| a.id.cmp(&b.id));
        let components = graph
            .nodes
            .iter()
            .filter_map(|n| Some((n.id.clone(), compiled.contract_for(&n.id)?.id.clone())))
            .collect();
        let mut connected = IndexVec::from_fn(ports.inputs().len(), |_| None);
        for (port, conn) in &wiring.writer {
            if let Some(input) = ports.input(&port.node, &port.port) {
                connected[input] = Some(conn.id.clone());
            }
        }

        let wired = plan::node_wiring(&compiled, &wiring, &ports);
        let wakes = Arc::new(WakeSet::new(islands.len()));
        let mut slots = Vec::with_capacity(islands.len());
        let members = compiled.islands().iter();
        for (index, ((members, (live, shapes)), wired)) in
            members.zip(islands).zip(wired).enumerate()
        {
            let plan = plan::plan_island(&compiled, &wiring, &ports, index, members, &shapes)?;
            // A new island owes its first generation.
            let mut owed = Owed::default();
            owed.push_latched();
            slots.push(Slot {
                required: plan.required_inputs(),
                awaiting: vec![false; plan.members.len()],
                plan: Arc::new(plan),
                offset: wired.offset,
                preds: wired.preds,
                feedback_sources: wired.feedback_sources,
                feedback_out: wired.feedback_out,
                stale_feedback: PortSet::new(ports.feedback().len()),
                waker: futures::task::waker(Arc::new(IslandWaker {
                    island: index,
                    set: wakes.clone(),
                })),
                state: Island::new(live).into(),
                owed,
            });
        }
        drop(wiring);

        let island_index = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| (slot.plan.sorted_members(), index))
            .collect();
        let (events_tx, events_rx) = mpsc::unbounded();
        Ok(Self {
            compiled,
            mode,
            executor,
            max_steps_per_tick,
            slots,
            running: 0,
            island_index,
            inputs: IndexVec::from_fn(ports.inputs().len(), |_| None),
            outputs: IndexVec::from_fn(ports.outputs().len(), |_| None),
            feedback: IndexVec::from_fn(ports.feedback().len(), |_| None),
            ports,
            island_of_node,
            out_edges,
            feedback_into,
            connections,
            components,
            links,
            connected,
            faults: BTreeMap::new(),
            wakes,
            dirty: true,
            events_tx,
            events_rx,
            resources,
        })
    }
}

/// A good default for [`Scheduler::max_steps_per_tick`]: high enough that
/// only a graph that keeps starting work reaches it.
pub const DEFAULT_MAX_STEPS_PER_TICK: usize = 10_000;

/// Checks a [`Scheduler::max_steps_per_tick`] as [`Scheduler::new`] does:
/// at least 1 ([`RuntimeError::InvalidConfig`]).
pub fn check_max_steps_per_tick(steps: usize) -> Result<(), RuntimeError> {
    if steps == 0 {
        return Err(RuntimeError::InvalidConfig {
            message: "max_steps_per_tick must be at least 1".into(),
        });
    }
    Ok(())
}

/// Checks every island's resource claims as [`Scheduler::new`] does, so an
/// executor can reject a graph before it instantiates anything.
pub fn check_resources(compiled: &CompiledGraph) -> Result<(), RuntimeError> {
    register_resources(compiled).map(|_| ())
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

/// Dropping a graph drops every generation in flight; each is reported
/// stopped, so every `GenerationStarted` a mode saw has its end.
impl<M: RuntimeMode, X: Executor> Drop for Scheduler<M, X> {
    fn drop(&mut self) {
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(generation) = slot.state.running_generation() {
                self.mode.on_generation_stopped(index, generation);
            }
        }
    }
}
