//! The scheduler: a compiled graph's islands, their latched values and
//! the work they owe, driven over an [`Executor`].
//!
//! [`Scheduler`] is a typed layer: it holds the [`Executor`] and the latched
//! values ([`crate::latches`]), and the scheduling logic beside it, in
//! the crate's `control` module, never sees a value.
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

use std::future::Future;
use std::sync::Arc;

use witgraph_ir::{CompiledGraph, InputIdx, NodeId, PortDirection, PortName};

use witgraph_traits::PortVersion;

use crate::control::SchedulerCore;
use crate::error::RuntimeError;
use crate::executor::{Canonical, DataPlane, Executor, LiveIsland};
use crate::latches::{HostPlane, LatchReader};
use crate::mode::RuntimeMode;
use crate::node::NodeState;
use crate::plan::RunShape;
use crate::schedule::{FaultReport, TickResult};

/// A compiled graph being run: the last stage of the typestate chain
/// `GraphBuilder -> Graph -> CompiledGraph`, over an [`Executor`].
///
/// `M` is the instrumentation mode; `X` runs the nodes.
pub struct Scheduler<M: RuntimeMode, X: Executor> {
    mode: Arc<M>,
    pub(crate) core: SchedulerCore<HostPlane<X>>,
}

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
    /// The scheduling logic, over its data plane as it sees it.
    pub(crate) fn core(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        &mut self.core
    }

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
        let mode = Arc::new(mode);
        let islands = islands
            .into_iter()
            .map(|(live, shapes)| (Box::new(live) as Box<dyn LiveIsland>, shapes))
            .collect();
        let core = SchedulerCore::new(
            compiled,
            mode.clone(),
            islands,
            max_steps_per_tick,
            |compiled, ports, plans, events| {
                HostPlane::new(executor, compiled, ports, plans, events)
            },
        )?;
        Ok(Self { mode, core })
    }

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
        let val = self
            .core
            .plane
            .executor
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
        self.core().drain_events();
        let changed = self
            .core
            .plane
            .lock()
            .set_input(input, Arc::new(val.into_inner()));
        if changed {
            self.core().host_set(input);
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
        let val = self
            .core
            .plane
            .executor
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
        self.core().drain_events();
        let (input, _) = self.host_input(node, port)?;
        let cleared = self.core.plane.lock().clear_input(input);
        if cleared {
            self.core().host_cleared(input);
        }
        Ok(())
    }

    /// Makes the node's island owe a run on its latched inputs, as an input
    /// change would; a stopped island is rebuilt first. This is how an
    /// island whose inputs the host cannot change (a source driven by a
    /// capability, say) runs again after a fault, cancel or shutdown.
    pub fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.core().rerun(node)
    }

    /// What reads this graph's latched outputs at any time, while a tick
    /// holds the scheduler too.
    pub fn reader(&self) -> LatchReader<X::Value> {
        LatchReader {
            latches: self.core.plane.latches.clone(),
            ports: self.core.ports.clone(),
            versions: self.core.versions.clone(),
        }
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
        self.reader().read_output(node, port)
    }

    /// Which value of a Value or handle output port is latched: it moves on
    /// each time the node's `run` writes the port (or a snapshot is
    /// restored), so a host can tell it has seen the latest without reading
    /// it. The same errors as [`read_output`](Self::read_output).
    pub fn output_version(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<PortVersion, RuntimeError> {
        self.reader().output_version(node, port)
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

    /// Checks that `node` exists and `port` is a Value port of it in
    /// `direction`, and finds its type.
    fn value_port(
        &self,
        node: &NodeId,
        port: &PortName,
        direction: PortDirection,
    ) -> Result<&X::Type, RuntimeError> {
        if self.core.compiled.island_of(node).is_none() {
            return Err(RuntimeError::UnknownNode { node: node.clone() });
        }
        let ports = &self.core.ports;
        let entry = match direction {
            PortDirection::Input => ports.input(node, port).map(|input| &ports.inputs()[input]),
            PortDirection::Output => ports
                .output(node, port)
                .map(|output| &ports.outputs()[output]),
        };
        entry
            .and_then(|entry| self.core.plane.executor.port_type(&entry.port, direction))
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction,
            })
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
        let input = self.core.ports.input(node, port).ok_or_else(not_a_port)?;
        if let Some(connection) = &self.core.connected[input] {
            return Err(RuntimeError::ConnectedInput {
                node: node.clone(),
                port: port.clone(),
                connection: connection.clone(),
            });
        }
        Ok((input, ty))
    }

    /// A node's current lifecycle state: a view of its island's state at
    /// this moment (see [`crate::node`]). An unknown node is an error
    /// ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let core: &SchedulerCore<dyn DataPlane> = &self.core;
        core.node_state(node)
    }

    /// Takes the faults reported since the last call: the latest fault of
    /// each island that faulted, in island order. A tick ends early only
    /// for `fatal` ([`TickResult::Aborted`]); every other fault (a trap,
    /// exhausted fuel, a limit, a failed rebuild) is reported here, and in
    /// [`node_state`](Self::node_state) while the island stays faulted.
    pub fn take_faults(&mut self) -> Vec<FaultReport> {
        self.core().take_faults()
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
        self.core().cancel(node)
    }

    /// Stops everything: drops every in-flight generation and every Store,
    /// forgets all owed work and every buffered feedback value, and cancels
    /// every island that is still live. A faulted island keeps its fault. A
    /// later input change rebuilds the affected island as after
    /// [`cancel`](Self::cancel).
    pub fn shutdown(&mut self) {
        self.core().shutdown();
    }

    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections.
    ///
    /// Starts every island that can start, drives all in-flight
    /// generations concurrently, delivers Value outputs as `run`s return,
    /// and keeps starting islands whose inputs changed, until nothing is in
    /// flight and nothing can start. Then each feedback connection's latest
    /// value is written to its target (one loop iteration). A tick that
    /// returns [`TickResult::StepLimitReached`] or [`TickResult::Aborted`]
    /// latches nothing; a later tick finishes the iteration. Faults other
    /// than `fatal` do not end the tick: see
    /// [`take_faults`](Self::take_faults).
    ///
    /// An island whose generation never finishes (an endless stream, say)
    /// keeps the tick running: drive such a graph with
    /// [`tick_until`](Self::tick_until), or [`cancel`](Self::cancel) the
    /// node. Dropping the tick's future is safe and ends it as
    /// `tick_until` ends an interrupted one: in-flight generations persist
    /// and resume on the next tick.
    pub async fn tick(&mut self) -> TickResult {
        self.core().tick().await
    }

    /// Like [`tick`](Self::tick), but ends early when `stop` completes (a
    /// timer, say), with [`TickResult::Interrupted`]. In-flight generations
    /// carry on with the next tick. Every buffered feedback value whose
    /// iteration is over is latched as the tick ends: the value's target
    /// island has settled (it is not running, cannot start, and nothing
    /// upstream of it can), and so has every feedback source into it. So a
    /// feedback loop advances one iteration per tick even beside an island
    /// that never finishes.
    pub async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        self.core().tick_until(stop).await
    }

    /// The compiled graph being run.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.core.compiled
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// What runs the nodes.
    pub fn executor(&self) -> &X {
        &self.core.plane.executor
    }

    /// What runs the nodes, to change state it shares with islands it
    /// builds later.
    pub fn executor_mut(&mut self) -> &mut X {
        &mut self.core.plane.executor
    }

    /// The most generations one tick may start before it returns
    /// [`TickResult::StepLimitReached`].
    pub fn max_steps_per_tick(&self) -> usize {
        self.core.max_steps_per_tick
    }

    /// Sets [`max_steps_per_tick`](Self::max_steps_per_tick); at least 1
    /// ([`RuntimeError::InvalidConfig`]).
    pub fn set_max_steps_per_tick(&mut self, steps: usize) -> Result<(), RuntimeError> {
        crate::control::check_max_steps_per_tick(steps)?;
        self.core.max_steps_per_tick = steps;
        Ok(())
    }
}
