//! The scheduling logic, which holds no values: what to run when, over a
//! [`DataPlane`]. It knows ports by number, through its [`PortTable`], and is
//! told what a `run` changed, never to what.
//!
//! [`SchedulerCore`] ends in its data plane, so that a typed wrapper keeps its
//! own plane's type, while everything here is compiled once, for
//! `dyn DataPlane`.
//!
//! # Scheduling model
//!
//! The unit of execution is the node. Each has a lifecycle (`Waiting`,
//! `Scheduled`, `Running`, in the crate-private `lifecycle` module), and all of
//! them share one instance of the graph: one fault domain. Nodes joined by
//! stream or future connections form a *stream group*, since a stream can be read
//! once: its producer and reader must always re-run together.
//!
//! The data plane keeps one latched value per Value input port (written by
//! connections and `inject`) and one per Value output port (latched when the
//! node's `run` returns). The scheduler keeps which inputs have a value, each
//! output's version, and what is owed. Invariants:
//!
//! - A newly built instance owes every node a run. An external Value input that
//!   changes (value equality decides) makes its node owe a run, whatever phase it
//!   is in; any number of changes before the next start owe one run. Owing a
//!   member of a stream group owes the whole group.
//! - A node starts when it owes a run, it is not running, every required input
//!   has a value, its resource claims fit, and nothing *upstream* of it can still
//!   change its inputs (computed by `unsettled`). That keeps a node from running
//!   on half-updated inputs, while a Value latched by a stream group that never
//!   finishes still wakes everything downstream of it.
//! - A stream group starts as one: when a member can start, and every member has
//!   its required inputs, each of them is scheduled, and runs when what it reads
//!   has settled. Its claims are held until it has drained.
//! - A `run` in flight is never pre-empted: inputs that change while it runs are
//!   owed, and the node re-runs after it returns. A node reads its inputs when it
//!   starts.
//! - A stream group whose `run`s have all returned is *draining*: its writers may
//!   be going on. It cannot start again until the guest work its `run`s left is
//!   over, which the data plane tells ([`Command::AwaitDrained`]), whatever else
//!   goes on beside it. A data plane that cannot tell whose work is whose drains
//!   it only once no guest work is left in the whole instance, so beside an
//!   endless stream it never re-runs there.
//! - A Value output is delivered as soon as its `run` returns. One reaching a
//!   member of the same group over a non-feedback connection owes nothing: it is
//!   part of the group's run.
//! - Feedback connections (values and handles) are latched once per tick, at quiescence:
//!   that is one iteration of a loop. A tick that ends early (`StepLimitReached`,
//!   `Faulted`) latches nothing; a later tick finishes the iteration. A dropped or
//!   interrupted tick latches, as it ends, the feedback into every node whose
//!   iteration is over: it has settled, and so has every feedback source into it.
//! - A host write that changes an input wins over older feedback: it drops
//!   feedback buffered for that input, and what a `run` in flight at the source
//!   would feed back into it is dropped too.
//! - A fault (trap, fuel exhaustion, a limit, `fatal`) stops the whole instance
//!   and faults every node, recording the culprit when it is known and the fault
//!   for `take_faults`. Every value is gone but the inputs the host wrote. What
//!   the host asked for since the instance started, and has not started since,
//!   is still owed, so an input that changed while it happened rebuilds the
//!   instance; otherwise a stopped graph waits for its next input change or
//!   `rerun`. So a node that always traps cannot rebuild the graph forever. A new
//!   instance owes every node a run, and resets feedback loops. A rebuild that
//!   fails faults every node with
//!   [`NodeFault::Restart`](crate::NodeFault::Restart).
//! - Shutdown drops the instance and forgets owed work.
//! - Commands to the data plane are served in order. A start pass needs proof
//!   ([`Drained`](crate::control)) that every one is answered, so nothing starts
//!   on inputs a latch is yet to write.
//!
//! What is in flight lives in the scheduler, not in the future returned by
//! `tick`: dropping a tick (on a timeout, say) loses nothing, and the next tick
//! resumes it. Every `&mut self` entry point first handles what the instance
//! reported since the last tick, so a `run` that returned before a dropped tick
//! or a shutdown still has its changes recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::future::Either;
use futures::stream::StreamExt;
use witgraph_ir::{
    CompiledGraph, Connection, ConnectionId, ContractId, FeedbackIdx, Idx, IndexVec, InputIdx,
    Link, NodeId, NodeIdx, OutputIdx, PortKind, PortName, PortSet, PortTable, ResourceId,
};
use witgraph_traits::{
    Command, ControlEvent, ControlKind, DataPlane, FaultReport, Incarnation, Latched, NodePhase,
    NodeState, Outcome, PortVersion, Returned, RuntimeError, RuntimeMode, Serving, Snapshot,
    TickResult,
};

use crate::lifecycle::{GraphState, Group, GroupLife, Instance, Node, NodeLife, StopCause};
use crate::resource::{ResourcePool, Room};

/// Every output port's version, shared: readable while a tick holds the
/// scheduler.
#[derive(Debug, Clone)]
pub struct PortVersions(Arc<[AtomicU64]>);

impl PortVersions {
    fn new(ports: usize) -> Self {
        Self((0..ports).map(|_| AtomicU64::new(0)).collect())
    }

    /// The version of `output`.
    pub fn get(&self, output: OutputIdx) -> PortVersion {
        PortVersion::new(self.0[output.position()].load(Ordering::Relaxed))
    }

    fn bump(&self, output: OutputIdx) {
        self.0[output.position()].fetch_add(1, Ordering::Relaxed);
    }

    fn bump_all(&self) {
        for version in self.0.iter() {
            version.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One node's wiring and state.
struct NodeEntry {
    /// The nodes writing one of its inputs over a non-feedback connection.
    preds: Box<[NodeIdx]>,
    /// The nodes writing one of its inputs over a feedback connection.
    feedback_sources: Box<[NodeIdx]>,
    /// The feedback connections out of it.
    feedback_out: Box<[FeedbackIdx]>,
    /// The inputs that must hold a value before it may start: for a group's
    /// member, those nothing in the group writes.
    required: Box<[InputIdx]>,
    group: Option<usize>,
    life: NodeLife,
}

struct GroupEntry {
    members: Box<[NodeIdx]>,
    /// What is above a member and not below any: the group does not start while one
    /// of these could still run. Its producers would hold its claims, and block on
    /// readers yet to start, and with them every group waiting to count as drained.
    /// What is below a member is left out, as it may take this group's start to run.
    before: Box<[NodeIdx]>,
    life: GroupLife,
    /// The number the last [`Command::AwaitDrained`] of it carried, so an answer meant
    /// for an earlier drain ends no later one.
    drain_asked: u64,
}

/// Proof that no reply to a command is outstanding: a start pass needs one, so
/// that nothing starts on inputs a latch is yet to write.
struct Drained(());

/// What the tick loop woke up for.
enum Wake {
    Event(ControlEvent),
    Stopped(Outcome),
}

/// What a start pass did.
enum Started {
    Nothing,
    Some,
    /// It hit `max_steps_per_tick` with work still waiting to start.
    LimitReached,
}

/// The scheduling state of a compiled graph being run, ending in its data
/// plane `P` (see the [module docs](self)).
pub struct SchedulerCore<P: ?Sized> {
    compiled: CompiledGraph,
    mode: Arc<dyn RuntimeMode>,
    max_steps_per_tick: usize,
    ports: Arc<PortTable>,
    nodes: IndexVec<NodeIdx, NodeEntry>,
    groups: Vec<GroupEntry>,
    /// The nodes owing a run.
    owed: PortSet<NodeIdx>,
    /// The input ports holding a value. Only Value and handle ports ever do,
    /// and streams and futures until their reader starts.
    presence: PortSet<InputIdx>,
    /// The inputs written inside a stream group: a change owes nothing.
    internal: PortSet<InputIdx>,
    /// The inputs the host wrote: all that survives the instance.
    host_written: PortSet<InputIdx>,
    /// The inputs with a host write or clear on its way, whoever waits for it.
    writing: PortSet<InputIdx>,
    /// Each output port's version.
    versions: PortVersions,
    /// The feedback connections with a value waiting for the end-of-iteration
    /// latch.
    pending: PortSet<FeedbackIdx>,
    /// Feedback connections whose target the host wrote since their source's
    /// `run` in flight started: what it feeds back there is stale.
    stale: PortSet<FeedbackIdx>,
    /// The feedback connections into each input port.
    feedback_into: IndexVec<InputIdx, Box<[FeedbackIdx]>>,
    /// The non-feedback connection writing each input port, if one does: the
    /// one writer the host may not compete with.
    connected: IndexVec<InputIdx, Option<ConnectionId>>,
    /// The graph's connections, sorted by id: how snapshots name the wiring.
    connections: Vec<Connection>,
    /// Each node's resolved component: how snapshots name the nodes.
    components: BTreeMap<NodeId, ContractId>,
    /// The graph's links, sorted by id.
    links: Vec<Link>,
    /// The latest fault not yet taken.
    fault: Option<FaultReport>,
    state: Option<GraphState>,
    incarnation: Incarnation,
    /// The nodes the host asked to run since the instance started, by writing
    /// an input or `rerun`, and that have not started since. All that is still
    /// owed after a fault: a new instance runs everything, and if it were built
    /// for less, a node that always traps would rebuild the graph forever.
    owed_by_host: PortSet<NodeIdx>,
    /// Whether a new instance is yet to report it was built: what is asked of
    /// it is queued, and has not started if its build fails.
    building: bool,
    /// How many `run`s are in flight.
    running: usize,
    /// How many commands are yet to be answered.
    outstanding: usize,
    /// How many `run`s were ever started: what a report of no guest work left
    /// is good for.
    runs_started: u64,
    /// The `runs_started` a report of no guest work left was asked at, while
    /// one is awaited.
    quiet_asked: Option<u64>,
    /// No guest work has been left since the last `run` started.
    quiet: bool,
    /// Whether anything changed since the last start pass that could let a node
    /// start.
    dirty: bool,
    events_tx: UnboundedSender<ControlEvent>,
    events_rx: UnboundedReceiver<ControlEvent>,
    resources: ResourcePool,
    /// What holds the values and runs the nodes. Last, so that
    /// `SchedulerCore<P>` coerces to `SchedulerCore<dyn DataPlane>`.
    pub plane: P,
}

/// Ends a tick that did not run to its end (it was dropped, say) the way
/// [`SchedulerCore::tick_until`] ends an interrupted one.
struct TickGuard<'a> {
    core: &'a mut SchedulerCore<dyn DataPlane>,
    finished: bool,
}

impl Drop for TickGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.core.tick_interrupted();
        }
    }
}

/// What is above any of `members` and below none, by `preds`. A group runs as one, so what
/// is above a member of a group above is above too. One member of another
/// group below makes all of it below: that group may need this one to start, so holding
/// this one back for it could leave each waiting for the other.
fn before_group(
    members: &[NodeIdx],
    preds: &[BTreeSet<NodeIdx>],
    groups: &[Box<[NodeIdx]>],
) -> Box<[NodeIdx]> {
    let mut above: BTreeSet<NodeIdx> = BTreeSet::new();
    let mut waiting: Vec<NodeIdx> = members.to_vec();
    while let Some(node) = waiting.pop() {
        let mates = groups.iter().find(|group| group.contains(&node));
        let mates = mates.into_iter().flat_map(|group| group.iter());
        for next in preds[node.position()].iter().chain(mates) {
            if above.insert(*next) {
                waiting.push(*next);
            }
        }
    }
    let mut below: BTreeSet<NodeIdx> = members.iter().copied().collect();
    loop {
        let before = below.len();
        // Nodes are numbered in topological order, so one pass follows the connections.
        for (at, of_node) in preds.iter().enumerate() {
            if of_node.iter().any(|pred| below.contains(pred)) {
                below.insert(NodeIdx::from_position(at));
            }
        }
        for group in groups {
            if group.iter().any(|member| below.contains(member)) {
                below.extend(group.iter().copied());
            }
        }
        if below.len() == before {
            return above.difference(&below).copied().collect();
        }
    }
}

impl<P: DataPlane> SchedulerCore<P> {
    /// A scheduler over a compiled graph, with the data plane `make_plane` makes
    /// from its numbered ports. Every node owes its first run. An instance
    /// starts being served at once.
    pub fn new(
        compiled: CompiledGraph,
        mode: Arc<dyn RuntimeMode>,
        max_steps_per_tick: usize,
        make_plane: impl FnOnce(&CompiledGraph, &Arc<PortTable>) -> P,
    ) -> Result<Self, RuntimeError> {
        check_max_steps_per_tick(max_steps_per_tick)?;
        let ports = Arc::new(PortTable::new(&compiled));
        let resources = register_resources(&compiled, &ports)?;
        let graph = compiled.graph();

        let node_of = |id: &NodeId| ports.node(id);
        let mut preds: Vec<BTreeSet<NodeIdx>> = vec![BTreeSet::new(); ports.node_count()];
        let mut feedback_sources: Vec<BTreeSet<NodeIdx>> =
            vec![BTreeSet::new(); ports.node_count()];
        let mut feedback_out: Vec<Vec<FeedbackIdx>> = vec![Vec::new(); ports.node_count()];
        let mut feedback_into: Vec<Vec<FeedbackIdx>> = vec![Vec::new(); ports.inputs().len()];
        let mut internal = PortSet::new(ports.inputs().len());
        let mut connected: IndexVec<InputIdx, Option<ConnectionId>> =
            IndexVec::from_fn(ports.inputs().len(), |_| None);
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (Some(from), Some(to), Some(input)) = (
                node_of(&conn.from.node),
                node_of(&conn.to.node),
                ports.input(&conn.to.node, &conn.to.port),
            ) else {
                continue;
            };
            match ports.feedback_index(&conn.id) {
                Some(feedback) => {
                    feedback_sources[to.position()].insert(from);
                    feedback_out[from.position()].push(feedback);
                    feedback_into[input.position()].push(feedback);
                }
                None => {
                    preds[to.position()].insert(from);
                    connected[input] = Some(conn.id.clone());
                    let (a, b) = (
                        compiled.group_of(&conn.from.node),
                        compiled.group_of(&conn.to.node),
                    );
                    if a.is_some() && a == b {
                        internal.insert(input);
                    }
                }
            }
        }
        let nodes: IndexVec<NodeIdx, NodeEntry> = (0..ports.node_count())
            .map(|at| {
                let id = ports.node_id(NodeIdx::from_position(at));
                let contract = compiled.contract_for(id);
                let required: Vec<InputIdx> = contract
                    .into_iter()
                    .flat_map(|c| &c.inputs)
                    .filter(|port| port.kind().is_latched() && !port.is_optional())
                    .filter_map(|port| ports.input(id, &port.name))
                    .filter(|input| !internal.contains(*input))
                    .collect();
                NodeEntry {
                    preds: preds[at].iter().copied().collect(),
                    feedback_sources: feedback_sources[at].iter().copied().collect(),
                    feedback_out: std::mem::take(&mut feedback_out[at]).into_boxed_slice(),
                    required: required.into_boxed_slice(),
                    group: compiled.group_of(id),
                    life: NodeLife::Waiting(Node::fresh()),
                }
            })
            .collect();
        let of_groups: Vec<Box<[NodeIdx]>> = compiled
            .stream_groups()
            .iter()
            .map(|members| members.iter().filter_map(node_of).collect())
            .collect();
        let groups = of_groups
            .iter()
            .map(|members| GroupEntry {
                before: before_group(members, &preds, &of_groups),
                members: members.clone(),
                life: GroupLife::Quiet(Group::fresh()),
                drain_asked: 0,
            })
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

        let plane = make_plane(&compiled, &ports);
        let (events_tx, events_rx) = mpsc::unbounded();
        let (commands_tx, commands_rx) = mpsc::unbounded();
        let incarnation = Incarnation::default();
        let server = plane.serve(Serving {
            incarnation,
            commands: commands_rx,
            events: events_tx.clone(),
        });
        let mut owed = PortSet::new(ports.node_count());
        for at in 0..ports.node_count() {
            owed.insert(NodeIdx::from_position(at));
        }
        Ok(Self {
            compiled,
            mode,
            max_steps_per_tick,
            nodes,
            groups,
            owed,
            presence: PortSet::new(ports.inputs().len()),
            internal,
            host_written: PortSet::new(ports.inputs().len()),
            writing: PortSet::new(ports.inputs().len()),
            versions: PortVersions::new(ports.outputs().len()),
            pending: PortSet::new(ports.feedback().len()),
            stale: PortSet::new(ports.feedback().len()),
            feedback_into: feedback_into
                .into_iter()
                .map(Vec::into_boxed_slice)
                .collect(),
            connected,
            connections,
            components,
            links,
            fault: None,
            state: Some(GraphState::Live(Instance::new(commands_tx, server))),
            incarnation,
            owed_by_host: PortSet::new(ports.node_count()),
            building: false,
            running: 0,
            outstanding: 0,
            runs_started: 0,
            quiet_asked: None,
            quiet: false,
            dirty: true,
            events_tx,
            events_rx,
            resources,
            ports,
            plane,
        })
    }
}

impl<P: ?Sized> SchedulerCore<P> {
    /// The compiled graph being run.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// Every port and feedback connection, numbered.
    pub fn ports(&self) -> &Arc<PortTable> {
        &self.ports
    }

    /// Every output port's version.
    pub fn versions(&self) -> &PortVersions {
        &self.versions
    }

    /// The most runs one tick may start before it returns
    /// [`TickResult::StepLimitReached`].
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

    /// Whether an instance is up.
    pub fn is_live(&self) -> bool {
        matches!(self.state, Some(GraphState::Live(_)))
    }

    /// The non-feedback connection writing `input`, if one does.
    pub fn connected(&self, input: InputIdx) -> Option<&ConnectionId> {
        self.connected[input].as_ref()
    }

    /// Whether `input` holds a value.
    pub fn is_present(&self, input: InputIdx) -> bool {
        self.presence.contains(input)
    }
}

impl SchedulerCore<dyn DataPlane> {
    /// Sends a command, if an instance is up.
    fn send(&mut self, command: Command) -> bool {
        match &self.state {
            Some(GraphState::Live(instance)) => {
                instance.send(command);
                true
            }
            _ => false,
        }
    }

    fn owe(&mut self, node: NodeIdx) {
        self.dirty = true;
        match self.nodes[node].group {
            Some(group) => {
                for at in 0..self.groups[group].members.len() {
                    self.owed.insert(self.groups[group].members[at]);
                }
            }
            None => {
                self.owed.insert(node);
            }
        }
    }

    /// As [`owe`](Self::owe), because the host asked.
    fn owe_host(&mut self, node: NodeIdx) {
        self.owe(node);
        self.owed_by_host.insert(node);
    }

    /// `input` holds a new value. A node about to start will read it, so only a
    /// node past that owes another run.
    fn owe_for(&mut self, input: InputIdx) {
        let node = self.ports.inputs()[input].node;
        if matches!(self.nodes[node].life, NodeLife::Scheduled(_)) {
            self.dirty = true;
            return;
        }
        self.owe(node);
    }

    /// Records a host write that changed an input to a new value. It wins over
    /// feedback for this input computed before it.
    pub fn host_set(&mut self, input: InputIdx) {
        self.presence.insert(input);
        self.host_written.insert(input);
        self.owe_host(self.ports.inputs()[input].node);
        self.note_host_write(input);
    }

    /// Records a host write that cleared an input.
    pub fn host_cleared(&mut self, input: InputIdx) {
        self.presence.remove(input);
        self.host_written.remove(input);
        self.owe_host(self.ports.inputs()[input].node);
        self.note_host_write(input);
    }

    /// Records a host write of the value an input already holds: nothing runs for
    /// it, but the value is the host's now, and outlives the instance.
    pub fn host_kept(&mut self, input: InputIdx) {
        self.presence.insert(input);
        self.host_written.insert(input);
    }

    /// A host write to `input` has started on its way to the instance, which will
    /// report [`ControlKind::HostWrote`]. Whoever asked may give up on it, which
    /// does not stop it. Nothing starts before it arrives.
    pub fn host_write_started(&mut self, input: InputIdx) {
        self.outstanding += 1;
        self.writing.insert(input);
        self.presence.insert(input);
        self.host_written.insert(input);
    }

    /// As [`host_write_started`](Self::host_write_started), for a clear, reported
    /// as [`ControlKind::HostCleared`].
    pub fn host_clear_started(&mut self, input: InputIdx) {
        self.outstanding += 1;
        self.writing.insert(input);
        self.presence.remove(input);
        self.host_written.remove(input);
    }

    /// What a restore writes has started on its way to the new instance, which will
    /// report [`ControlKind::Restored`]. Nothing starts before it arrives.
    pub fn restore_writes_started(&mut self) {
        self.outstanding += 1;
    }

    fn note_host_write(&mut self, input: InputIdx) {
        for at in 0..self.feedback_into[input].len() {
            let conn = self.feedback_into[input][at];
            if self.pending.remove(conn) {
                self.send(Command::DropFeedback(conn));
            }
            let source = self.ports.outputs()[self.ports.feedback()[conn].from].node;
            if self.nodes[source].life.is_running() {
                self.stale.insert(conn);
            }
        }
    }

    /// Makes the node owe a run on its latched inputs, as an input change
    /// would; a stopped graph is rebuilt first. This is how a node whose inputs
    /// the host cannot change (a source driven by a capability, say) runs again
    /// after a fault or shutdown.
    pub fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.drain();
        let index = self
            .ports
            .node(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        self.owe_host(index);
        Ok(())
    }

    /// A node's current lifecycle state. An unknown node is an error
    /// ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let unknown = || RuntimeError::UnknownNode { node: node.clone() };
        let index = self.ports.node(node).ok_or_else(unknown)?;
        let contract = self.compiled.contract_for(node).ok_or_else(unknown)?;
        let (phase, fault, culprit) = match &self.state {
            Some(GraphState::Stopped(stopped)) => match stopped.cause() {
                StopCause::Faulted(fault, culprit) => {
                    (NodePhase::Faulted, Some(fault.clone()), culprit.clone())
                }
                StopCause::Shutdown => (NodePhase::Cancelled, None, None),
            },
            _ => (self.phase_of(index), None, None),
        };
        Ok(NodeState::new(
            node.clone(),
            contract.shape(),
            phase,
            fault,
            culprit,
        ))
    }

    /// Takes the fault reported since the last call, if there is one: the one a
    /// tick returned ([`TickResult::Faulted`]), or one no tick has seen yet.
    pub fn take_faults(&mut self) -> Vec<FaultReport> {
        self.drain();
        let polled = self.poll_server_once();
        if let Some(outcome) = polled {
            self.drain();
            self.stopped(outcome);
        }
        self.fault.take().into_iter().collect()
    }

    /// Whether the server has ended, polled once: a pull may have ended it while
    /// no tick was looking.
    fn poll_server_once(&mut self) -> Option<Outcome> {
        let Some(GraphState::Live(instance)) = &mut self.state else {
            return None;
        };
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        match instance.poll(&mut cx) {
            Poll::Ready(outcome) => Some(outcome),
            Poll::Pending => None,
        }
    }

    /// Stops everything: drops the instance, forgets all owed work, and a later
    /// input change rebuilds the graph. A faulted graph keeps its fault.
    pub fn shutdown(&mut self) {
        self.drain();
        self.owed.clear();
        self.owed_by_host.clear();
        self.writing.clear();
        if self.is_live() {
            self.stop(StopCause::Shutdown);
            for node in self.compiled.graph().nodes.iter().map(|n| n.id.clone()) {
                self.mode.on_cancelled(&node);
            }
        }
        self.dirty = true;
    }

    /// Handles every event already reported. Returns whether any did something.
    pub fn drain(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.events_rx.try_recv() {
            any |= self.handle_event(event);
        }
        any
    }

    /// Serves the commands already sent, then handles what that reported. A host
    /// that writes and reads its values directly, not through its server, calls
    /// this first, so that it acts after what an interrupted tick left queued, as
    /// on a host that queues them. An instance this stops has its fault in
    /// [`take_faults`](Self::take_faults).
    pub fn serve_queued(&mut self) {
        if let Some(outcome) = self.poll_server_once() {
            self.drain();
            let _ = self.stopped(outcome);
        }
        self.drain();
    }

    /// Takes in what happened since the last tick, starting nothing: a `run` that
    /// returned, and whether guest work has ended, so that phases say what is so between
    /// ticks. A graph left running by a tick that ended early can be followed with this.
    pub fn catch_up(&mut self) {
        self.serve_queued();
        if self.running == 0 && !self.quiet && self.quiet_asked.is_none() {
            self.quiet_asked = Some(self.runs_started);
            self.send(Command::AwaitQuiet(self.runs_started));
            self.serve_queued();
        }
    }

    fn drained(&self) -> Option<Drained> {
        (self.outstanding == 0).then_some(Drained(()))
    }

    fn handle_event(&mut self, event: ControlEvent) -> bool {
        if event.incarnation != self.incarnation {
            return false;
        }
        match event.kind {
            ControlKind::Built => {
                self.building = false;
                let started: Vec<NodeIdx> = self
                    .owed_by_host
                    .iter()
                    .filter(|node| !self.owed.contains(*node))
                    .collect();
                for node in started {
                    self.owed_by_host.remove(node);
                }
                let ids: Vec<NodeId> = self.ports_node_ids();
                for node in &ids {
                    self.mode.on_restarted(node);
                }
                false
            }
            ControlKind::RunReturned { node, returned } => {
                self.run_returned(node, returned);
                true
            }
            ControlKind::Latched {
                connection,
                latched,
            } => {
                self.outstanding = self.outstanding.saturating_sub(1);
                self.dirty = true;
                let to = self.ports.feedback()[connection].to;
                match latched {
                    Latched::Unchanged => false,
                    Latched::Changed => {
                        self.presence.insert(to);
                        self.owe_for(to);
                        true
                    }
                    Latched::Cleared => {
                        self.presence.remove(to);
                        self.owe_for(to);
                        true
                    }
                }
            }
            // Each has the last word on its input: a latch asked before it may have
            // said otherwise since it started.
            ControlKind::HostWrote { input, changed } => {
                self.presence.insert(input);
                self.host_write_arrived(input, changed)
            }
            ControlKind::HostCleared { input, cleared } => {
                self.presence.remove(input);
                self.host_write_arrived(input, cleared)
            }
            ControlKind::Restored => {
                self.outstanding = self.outstanding.saturating_sub(1);
                self.dirty = true;
                false
            }
            ControlKind::Drained { group, at } => {
                if self.groups.get(group).is_some_and(|g| g.drain_asked == at) {
                    self.end_group_draining(group);
                }
                false
            }
            ControlKind::Quiet(at) => {
                self.quiet_asked = None;
                if at == self.runs_started && self.running == 0 {
                    self.quiet = true;
                    self.end_draining();
                }
                false
            }
        }
    }

    /// A host write or clear reached its input: if that changed it, its node owes a
    /// run, and the write wins over feedback computed before it.
    fn host_write_arrived(&mut self, input: InputIdx, changed: bool) -> bool {
        self.outstanding = self.outstanding.saturating_sub(1);
        self.writing.remove(input);
        self.dirty = true;
        if changed {
            self.owe_host(self.ports.inputs()[input].node);
            self.note_host_write(input);
        }
        changed
    }

    fn ports_node_ids(&self) -> Vec<NodeId> {
        (0..self.ports.node_count())
            .map(|at| self.ports.node_id(NodeIdx::from_position(at)).clone())
            .collect()
    }

    /// No guest work is left: every draining group may run again.
    fn end_draining(&mut self) {
        for at in 0..self.groups.len() {
            self.end_group_draining(at);
        }
    }

    /// No guest work of group `at` is left: if draining, it may run again.
    fn end_group_draining(&mut self, at: usize) {
        if let GroupLife::Draining(group) = self.groups[at].life {
            self.groups[at].life = GroupLife::Quiet(group.drained());
            for member in self.groups[at].members.iter() {
                self.resources.release(member.position());
            }
            let nodes: Vec<NodeId> = self.groups[at]
                .members
                .iter()
                .map(|member| self.ports.node_id(*member).clone())
                .collect();
            for node in &nodes {
                self.mode
                    .on_phase_transition(node, NodePhase::Running, NodePhase::Idle);
            }
            self.mode.on_group_drained(&nodes);
            self.dirty = true;
        }
    }

    /// What `node` is doing. A member of a stream group that has yet to drain is still
    /// at work after its `run` returned: a `run` often only hands over a stream, and
    /// its writer goes on.
    fn phase_of(&self, node: NodeIdx) -> NodePhase {
        match self.nodes[node].life.phase() {
            NodePhase::Idle if !self.group_is_quiet(node) => NodePhase::Running,
            phase => phase,
        }
    }

    fn set_life(&mut self, node: NodeIdx, life: NodeLife) {
        let from = self.phase_of(node);
        self.nodes[node].life = life;
        let to = self.phase_of(node);
        if from != to {
            self.mode
                .on_phase_transition(self.ports.node_id(node), from, to);
        }
    }

    /// Records what a returned `run` changed.
    fn run_returned(&mut self, node: NodeIdx, returned: Returned) {
        let NodeLife::Running(running) = self.nodes[node].life else {
            return;
        };
        self.running -= 1;
        self.dirty = true;
        self.set_life(node, NodeLife::Waiting(running.returned()));
        self.mode.on_run_returned(self.ports.node_id(node));
        match self.nodes[node].group {
            Some(group) => {
                self.resources.linger(node.position());
                if let GroupLife::Active(active) = self.groups[group].life {
                    self.groups[group].life = active.returned();
                    if matches!(self.groups[group].life, GroupLife::Draining(_)) {
                        let entry = &mut self.groups[group];
                        entry.drain_asked += 1;
                        let (at, members) = (entry.drain_asked, entry.members.clone());
                        self.send(Command::AwaitDrained { group, at, members });
                    }
                }
            }
            None => self.resources.release(node.position()),
        }
        let Returned {
            outputs_changed,
            inputs_changed,
            inputs_recorded,
            inputs_cleared,
            feedback_buffered,
        } = returned;
        for output in outputs_changed {
            self.versions.bump(output);
        }
        for input in inputs_changed {
            self.presence.insert(input);
            self.owe_for(input);
        }
        for input in inputs_recorded {
            self.presence.insert(input);
        }
        for input in inputs_cleared {
            self.presence.remove(input);
            if !self.internal.contains(input) {
                self.owe_for(input);
            }
        }
        for conn in feedback_buffered {
            // Computed from inputs older than a host write to its target: the
            // host's value stands.
            if self.stale.contains(conn) {
                self.send(Command::DropFeedback(conn));
            } else {
                self.pending.insert(conn);
            }
        }
    }

    /// Whether what `node` starts with is there: for a group's member, what
    /// every member starts with. A draining group cannot start yet.
    fn ready(&self, node: NodeIdx) -> bool {
        let required = |at: NodeIdx| {
            self.nodes[at]
                .required
                .iter()
                .all(|input| self.presence.contains(*input))
        };
        match self.nodes[node].group {
            Some(group) => {
                let entry = &self.groups[group];
                !matches!(entry.life, GroupLife::Draining(_))
                    && entry.members.iter().all(|member| required(*member))
            }
            None => required(node),
        }
    }

    /// Whether `node`'s stream group, if it has one, has nothing in flight.
    fn group_is_quiet(&self, node: NodeIdx) -> bool {
        self.nodes[node]
            .group
            .is_none_or(|group| matches!(self.groups[group].life, GroupLife::Quiet(_)))
    }

    /// The nodes that can still produce new outputs, and the nodes with one of
    /// those upstream, which must not start yet. A node waits while what it
    /// reads could change, which keeps it from running on half-updated inputs,
    /// while a Value latched by a stream group that never finishes still wakes
    /// what is downstream of it. A group whose `before` could still run counts as
    /// able to change too, and does not start. Each pass is linear in the size of
    /// the graph, and only such a group takes another.
    ///
    /// The last set is everything that can still run, for what holds a group back: the
    /// unsettled, and each member of a group that may yet owe a run or owes one it cannot
    /// start yet. Nothing else waits for those, as what such a group waits for may read
    /// them: `before` leaves out what is below it. What only its own members do does not
    /// count, or a group below an endless stream would never start.
    fn unsettled(&self) -> (PortSet<NodeIdx>, PortSet<NodeIdx>, PortSet<NodeIdx>) {
        let count = self.ports.node_count();
        let mut unsettled = PortSet::new(count);
        let mut blocked = PortSet::new(count);
        let mut can_run = PortSet::new(count);
        loop {
            let mut grew = false;
            // Nodes are numbered in topological order.
            for at in 0..count {
                let node = NodeIdx::from_position(at);
                let entry = &self.nodes[node];
                let upstream = entry.preds.iter().any(|pred| unsettled.contains(*pred));
                if upstream {
                    blocked.insert(node);
                }
                let own = match entry.life {
                    NodeLife::Running(_) | NodeLife::Scheduled(_) => true,
                    // A member owing a run of a group that has yet to drain cannot
                    // run until its group's readers have, so they must not wait.
                    NodeLife::Waiting(_) => {
                        self.group_is_quiet(node)
                            && ((self.owed.contains(node) && self.ready(node))
                                || self.group_is_held_back(node, &can_run))
                    }
                };
                if own || upstream {
                    grew |= unsettled.insert(node);
                    grew |= can_run.insert(node);
                }
                if let Some(group) = entry.group {
                    let outside = entry.preds.iter().any(|pred| {
                        unsettled.contains(*pred) && self.nodes[*pred].group != Some(group)
                    });
                    if outside || (self.owed.contains(node) && !self.group_is_quiet(node)) {
                        for member in &self.groups[group].members {
                            grew |= can_run.insert(*member);
                        }
                    }
                }
            }
            if !grew {
                return (unsettled, blocked, can_run);
            }
        }
    }

    /// Whether `node` is in a group owing a run with something in its `before` that
    /// could still run.
    fn group_is_held_back(&self, node: NodeIdx, can_run: &PortSet<NodeIdx>) -> bool {
        self.nodes[node].group.is_some_and(|group| {
            let entry = &self.groups[group];
            entry
                .members
                .iter()
                .any(|member| self.owed.contains(*member))
                && entry.before.iter().any(|up| can_run.contains(*up))
        })
    }

    /// Builds a new instance, if work is owed that could start. Every node owes
    /// a run in it: its values are gone with the last one.
    pub fn rebuild_if_owed(&mut self) -> bool {
        let wanted = self.owed.iter().any(|node| self.ready(node));
        if !wanted {
            return false;
        }
        let Some(GraphState::Stopped(stopped)) = self.state.take() else {
            return false;
        };
        let (tx, rx) = mpsc::unbounded();
        self.incarnation = self.incarnation.next();
        let server = self.plane.serve(Serving {
            incarnation: self.incarnation,
            commands: rx,
            events: self.events_tx.clone(),
        });
        self.state = Some(GraphState::Live(stopped.restart(tx, server)));
        self.owed_by_host.clear();
        self.building = true;
        for at in 0..self.ports.node_count() {
            self.owed.insert(NodeIdx::from_position(at));
        }
        true
    }

    /// Starts every node that can start, in topological order, in one pass:
    /// starting one never lets another start. Each start counts as a step.
    fn start_ready(&mut self, steps: &mut usize, _: Drained) -> Started {
        if !self.dirty {
            return Started::Nothing;
        }
        self.dirty = false;
        if self.owed.is_empty()
            && self
                .groups
                .iter()
                .all(|g| !matches!(g.life, GroupLife::Active(_)))
        {
            return Started::Nothing;
        }
        if !self.is_live() && !self.rebuild_if_owed() {
            return Started::Nothing;
        }
        let (_, blocked, can_run) = self.unsettled();
        let awaited = self.awaited();
        let mut started = Started::Nothing;
        for at in 0..self.ports.node_count() {
            let node = NodeIdx::from_position(at);
            if blocked.contains(node) {
                continue;
            }
            let activate = match self.nodes[node].life {
                NodeLife::Running(_) => continue,
                NodeLife::Scheduled(_) => {
                    if !self.fits(node, &awaited) {
                        continue;
                    }
                    false
                }
                NodeLife::Waiting(_) => {
                    if !self.owed.contains(node) || !self.ready(node) {
                        continue;
                    }
                    // A group runs as one: its members start together, or not
                    // until it has drained, or while what it reads could change.
                    if !self.group_is_quiet(node) || self.group_is_held_back(node, &can_run) {
                        continue;
                    }
                    if !self.fits(node, &awaited) {
                        continue;
                    }
                    true
                }
            };
            if *steps >= self.max_steps_per_tick {
                // Work is still waiting to start.
                self.dirty = true;
                return Started::LimitReached;
            }
            self.resources.acquire(node.position());
            if activate {
                self.schedule_group(node);
            }
            *steps += 1;
            started = Started::Some;
            self.start_run(node);
        }
        started
    }

    /// If `node` is in a stream group, every member is now to run, each when
    /// what it reads has settled.
    fn schedule_group(&mut self, node: NodeIdx) {
        let Some(group) = self.nodes[node].group else {
            return;
        };
        let GroupLife::Quiet(quiet) = self.groups[group].life else {
            return;
        };
        let members = self.groups[group].members.clone();
        for member in &members {
            self.owed.remove(*member);
            if let NodeLife::Waiting(waiting) = self.nodes[*member].life {
                self.set_life(*member, NodeLife::Scheduled(waiting.schedule()));
            }
        }
        self.groups[group].life = GroupLife::Active(quiet.start(members.len()));
    }

    /// What a stream group at work is waiting on: each member yet to run, and all that
    /// is above it. The group cannot drain without them, as streams are not buffered.
    fn awaited(&self) -> PortSet<NodeIdx> {
        let mut awaited = PortSet::new(self.ports.node_count());
        let mut waiting: Vec<NodeIdx> = (0..self.ports.node_count())
            .map(NodeIdx::from_position)
            .filter(|node| matches!(self.nodes[*node].life, NodeLife::Scheduled(_)))
            .collect();
        while let Some(node) = waiting.pop() {
            if !awaited.insert(node) {
                continue;
            }
            let entry = &self.nodes[node];
            waiting.extend(entry.preds.iter().copied());
            if let Some(group) = entry.group {
                waiting.extend(self.groups[group].members.iter().copied());
            }
        }
        awaited
    }

    /// Whether `node`'s claims fit, given what the groups at work are waiting on.
    fn fits(&self, node: NodeIdx, awaited: &PortSet<NodeIdx>) -> bool {
        let room = if awaited.contains(node) {
            Room::PastLingering
        } else {
            Room::Free
        };
        self.resources.can_acquire(node.position(), room)
    }

    fn start_run(&mut self, node: NodeIdx) {
        self.owed.remove(node);
        if !self.building {
            self.owed_by_host.remove(node);
        }
        let running = match self.nodes[node].life {
            NodeLife::Waiting(waiting) => waiting.start(),
            NodeLife::Scheduled(scheduled) => scheduled.start(),
            NodeLife::Running(_) => return,
        };
        // Stale only for the run it was stale for.
        for at in 0..self.nodes[node].feedback_out.len() {
            let conn = self.nodes[node].feedback_out[at];
            self.stale.remove(conn);
        }
        self.set_life(node, NodeLife::Running(running));
        self.running += 1;
        self.runs_started += 1;
        self.quiet = false;
        self.mode.on_run_started(self.ports.node_id(node));
        self.send(Command::Run(node));
    }

    /// Stops the instance, and everything with it. All that survives is what
    /// the host wrote.
    fn stop(&mut self, cause: StopCause) {
        if let Some(GraphState::Live(instance)) = self.state.take() {
            self.state = Some(GraphState::Stopped(instance.stop(cause)));
        } else {
            return;
        }
        self.plane.stopped();
        // The next instance gets what they wrote, and their nodes owe a run on it.
        let unarrived: Vec<InputIdx> = self.writing.iter().collect();
        for input in unarrived {
            self.owed_by_host.insert(self.ports.inputs()[input].node);
        }
        self.writing.clear();
        self.running = 0;
        self.outstanding = 0;
        self.quiet_asked = None;
        self.quiet = false;
        self.building = false;
        for at in 0..self.ports.node_count() {
            let node = NodeIdx::from_position(at);
            self.nodes[node].life = NodeLife::Waiting(Node::fresh());
            self.resources.release(node.position());
        }
        for group in &mut self.groups {
            group.life = GroupLife::Quiet(Group::fresh());
        }
        self.presence = self.host_written.clone();
        self.pending.clear();
        self.stale.clear();
        self.versions.bump_all();
        self.dirty = true;
    }

    /// An instance's server ended, with what stopped it.
    fn stopped(&mut self, outcome: Outcome) -> Option<TickResult> {
        let Err((fault, caller)) = outcome else {
            // Only dropping what sends its commands ends one without a fault.
            self.stop(StopCause::Shutdown);
            return None;
        };
        // The one that called `fatal` or failed to build, or else the only one
        // with a `run` in flight, unless a stream group's writers may be at work
        // too.
        let in_flight: Vec<NodeIdx> = (0..self.ports.node_count())
            .map(NodeIdx::at)
            .filter(|node| self.nodes[*node].life.is_running())
            .collect();
        let writers_may_run = self
            .groups
            .iter()
            .any(|group| !matches!(group.life, GroupLife::Quiet(_)));
        let culprit = caller.or_else(|| match in_flight.as_slice() {
            [only] if !writers_may_run => Some(self.ports.node_id(*only).clone()),
            _ => None,
        });
        let ids = self.ports_node_ids();
        let phases: Vec<NodePhase> = (0..ids.len())
            .map(|at| self.nodes[NodeIdx::from_position(at)].life.phase())
            .collect();
        self.stop(StopCause::Faulted(fault.clone(), culprit.clone()));
        self.owed = self.owed_by_host.clone();
        for (node, from) in ids.iter().zip(phases) {
            if from != NodePhase::Faulted {
                self.mode
                    .on_phase_transition(node, from, NodePhase::Faulted);
            }
            self.mode.on_node_fault(node, &fault);
        }
        let report = FaultReport { culprit, fault };
        self.fault = Some(report.clone());
        Some(TickResult::Faulted(report))
    }

    /// Waits for the next report, or the server ending.
    async fn next_wake(&mut self) -> Wake {
        let Self {
            state, events_rx, ..
        } = self;
        poll_fn(|cx| {
            if let Poll::Ready(Some(event)) = events_rx.poll_next_unpin(cx) {
                return Poll::Ready(Wake::Event(event));
            }
            if let Some(GraphState::Live(instance)) = state
                && let Poll::Ready(outcome) = instance.poll(cx)
            {
                return Poll::Ready(Wake::Stopped(outcome));
            }
            // What serving it just reported, with no trip through the waker.
            match events_rx.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => Poll::Ready(Wake::Event(event)),
                _ => Poll::Pending,
            }
        })
        .await
    }

    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections.
    ///
    /// An instance whose work never finishes (an endless stream, say) keeps the
    /// tick running: drive such a graph with [`tick_until`](Self::tick_until).
    /// Dropping the tick's future is safe and ends it as `tick_until` ends an
    /// interrupted one: what is in flight resumes on the next tick.
    pub async fn tick(&mut self) -> TickResult {
        self.tick_until(std::future::pending()).await
    }

    /// Like [`tick`](Self::tick), but ends early when `stop` completes (a
    /// timer, say), with [`TickResult::Interrupted`]. What is in flight carries
    /// on with the next tick. Every buffered feedback value whose iteration is
    /// over is latched as the tick ends: the value's target has settled, and so
    /// has every feedback source into it. So a feedback loop advances one
    /// iteration per tick even beside work that never finishes.
    pub async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        let mut guard = TickGuard {
            core: self,
            finished: false,
        };
        let finished = {
            let run = guard.core.run_tick();
            futures::pin_mut!(run, stop);
            match futures::future::select(run, stop).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(((), _)) => None,
            }
        };
        guard.finished = true;
        match finished {
            Some(result) => result,
            None => {
                guard.core.tick_interrupted();
                TickResult::Interrupted
            }
        }
    }

    /// The body of a tick.
    async fn run_tick(&mut self) -> TickResult {
        let mut steps = 0usize;
        let mut progressed = false;
        if let Some(outcome) = self.poll_server_once() {
            self.drain();
            progressed = true;
            if let Some(faulted) = self.stopped(outcome) {
                return faulted;
            }
        }
        loop {
            progressed |= self.drain();
            if let Some(drained) = self.drained() {
                match self.start_ready(&mut steps, drained) {
                    Started::Nothing => {}
                    Started::Some => progressed = true,
                    Started::LimitReached => return TickResult::StepLimitReached,
                }
            }
            if !self.is_live() {
                break;
            }
            if self.running == 0 && self.outstanding == 0 && self.quiet && !self.dirty {
                break;
            }
            // Guest work left can only be told apart once no `run` is in flight.
            if self.running == 0 && !self.quiet && self.quiet_asked.is_none() {
                self.quiet_asked = Some(self.runs_started);
                self.send(Command::AwaitQuiet(self.runs_started));
            }
            match self.next_wake().await {
                Wake::Event(event) => progressed |= self.handle_event(event),
                Wake::Stopped(outcome) => {
                    self.drain();
                    progressed = true;
                    if let Some(faulted) = self.stopped(outcome) {
                        return faulted;
                    }
                }
            }
        }
        // One iteration of each feedback loop.
        self.latch(|_| true);
        while self.outstanding > 0 {
            match self.next_wake().await {
                Wake::Event(event) => progressed |= self.handle_event(event),
                Wake::Stopped(outcome) => {
                    progressed = true;
                    if let Some(faulted) = self.stopped(outcome) {
                        return faulted;
                    }
                }
            }
        }
        if progressed {
            TickResult::Progress
        } else {
            TickResult::Idle
        }
    }

    /// Ends a tick that stopped short of quiescence (dropped or interrupted):
    /// handles what its instance reported, and asks to latch the feedback of
    /// every iteration that is over.
    fn tick_interrupted(&mut self) {
        self.drain();
        if self.pending.is_empty() || !self.is_live() {
            return;
        }
        let (unsettled, blocked, _) = self.unsettled();
        let settled = |core: &Self, node: NodeIdx| {
            let entry = &core.nodes[node];
            !entry.life.is_running()
                && !(core.owed.contains(node) && core.ready(node))
                && !blocked.contains(node)
                && entry
                    .feedback_sources
                    .iter()
                    .all(|source| !unsettled.contains(*source))
        };
        let mut latch = PortSet::new(self.ports.feedback().len());
        for conn in self.pending.iter() {
            let target = self.ports.inputs()[self.ports.feedback()[conn].to].node;
            if settled(self, target) {
                latch.insert(conn);
            }
        }
        self.latch(|conn| latch.contains(conn));
    }

    /// Asks to write the buffered feedback values `which` picks to their targets.
    fn latch(&mut self, mut which: impl FnMut(FeedbackIdx) -> bool) {
        let conns: Vec<FeedbackIdx> = self.pending.iter().filter(|conn| which(*conn)).collect();
        for conn in conns {
            if self.send(Command::LatchFeedback(conn)) {
                self.pending.remove(conn);
                self.outstanding += 1;
            }
        }
    }

    /// What a snapshot says besides values: for a wrapper to add them.
    pub fn snapshot_base(&self) -> Snapshot {
        let ids = self.ports_node_ids();
        let mut in_flight = BTreeSet::new();
        let mut owed = BTreeSet::new();
        let mut phases = BTreeMap::new();
        for (at, id) in ids.iter().enumerate() {
            let node = NodeIdx::from_position(at);
            let entry = &self.nodes[node];
            phases.insert(
                id.clone(),
                self.node_state(id)
                    .map_or(NodePhase::Pending, |s| s.phase()),
            );
            match entry.life {
                NodeLife::Running(_) | NodeLife::Scheduled(_) => {
                    in_flight.insert(id.clone());
                }
                NodeLife::Waiting(_) => {}
            }
            if self.owed.contains(node) {
                owed.insert(id.clone());
            }
            // A handle is the host's, not text: what produced one owes a run,
            // which makes a new one.
            let makes_handle = self
                .compiled
                .contract_for(id)
                .is_some_and(|c| c.outputs.iter().any(|p| p.kind() == PortKind::Handle));
            if makes_handle && entry.life.has_run() {
                owed.insert(id.clone());
            }
        }
        let mut host_written: BTreeMap<NodeId, BTreeSet<PortName>> = BTreeMap::new();
        for input in self.host_written.iter() {
            let port = &self.ports.inputs()[input].port;
            host_written
                .entry(port.node.clone())
                .or_default()
                .insert(port.port.clone());
        }
        Snapshot {
            nodes: self.components.clone(),
            connections: self.connections.clone(),
            links: self.links.clone(),
            quiescent: self.running == 0,
            phases,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            feedback: BTreeMap::new(),
            host_written,
            in_flight,
            owed,
        }
    }

    /// Checks that `snapshot` can be restored onto this graph: nothing is in
    /// flight ([`RuntimeError::NotQuiescent`]), and it has exactly the snapshot's
    /// nodes, components, connections and links
    /// ([`RuntimeError::SnapshotMismatch`]).
    pub fn check_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.drain();
        if self.running > 0 {
            return Err(RuntimeError::NotQuiescent);
        }
        let mismatch = |message: String| RuntimeError::SnapshotMismatch { message };
        let ours = &self.components;
        if snapshot.nodes != *ours {
            let differs = ours
                .keys()
                .chain(snapshot.nodes.keys())
                .find(|n| ours.get(*n) != snapshot.nodes.get(*n))
                .map_or_else(|| "?".into(), |n| format!("node `{n}`"));
            return Err(mismatch(format!(
                "{differs} differs (missing, extra, or another component)"
            )));
        }
        if !snapshot.phases.keys().eq(ours.keys()) {
            return Err(mismatch(
                "the phases are not those of the graph's nodes".into(),
            ));
        }
        if snapshot.connections != self.connections {
            return Err(mismatch("the graph's connections differ".into()));
        }
        if snapshot.links != self.links {
            return Err(mismatch("the graph's links differ".into()));
        }
        if let Some(node) = snapshot
            .in_flight
            .iter()
            .chain(&snapshot.owed)
            .find(|node| !ours.contains_key(*node))
        {
            return Err(mismatch(format!("`{node}` is not a node of this graph")));
        }
        for (node, ports) in &snapshot.host_written {
            for port in ports {
                let writable = self
                    .ports
                    .input(node, port)
                    .is_some_and(|input| self.connected[input].is_none());
                if !writable {
                    return Err(mismatch(format!(
                        "the host cannot have written `{node}.{port}`"
                    )));
                }
            }
        }
        Ok(())
    }

    /// The inputs `snapshot` says the host wrote: what survives the next instance.
    pub fn host_written_in(&self, snapshot: &Snapshot) -> PortSet<InputIdx> {
        let mut written = PortSet::new(self.ports.inputs().len());
        for (node, ports) in &snapshot.host_written {
            for port in ports {
                if let Some(input) = self.ports.input(node, port) {
                    written.insert(input);
                }
            }
        }
        written
    }

    /// Replaces the instance with a new one, its nodes yet to run, for a restore
    /// to write its values into. Its guest state outlives nothing.
    pub fn restart_for_restore(&mut self) {
        let was_live = self.is_live();
        if was_live {
            self.stop(StopCause::Shutdown);
        }
        let Some(GraphState::Stopped(stopped)) = self.state.take() else {
            return;
        };
        let (tx, rx) = mpsc::unbounded();
        self.incarnation = self.incarnation.next();
        let server = self.plane.serve(Serving {
            incarnation: self.incarnation,
            commands: rx,
            events: self.events_tx.clone(),
        });
        self.state = Some(GraphState::Live(stopped.restart(tx, server)));
        self.owed_by_host.clear();
        self.building = true;
    }

    /// Puts a restored state in place: `presence` holds a value, `pending`
    /// feedback is buffered, and `snapshot`'s runs are owed.
    pub fn apply_restore(
        &mut self,
        presence: &[InputIdx],
        pending: &[FeedbackIdx],
        snapshot: &Snapshot,
    ) {
        let host_written = self.host_written_in(snapshot);
        self.presence = PortSet::new(self.ports.inputs().len());
        self.host_written = PortSet::new(self.ports.inputs().len());
        for input in presence {
            self.presence.insert(*input);
            if host_written.contains(*input) {
                self.host_written.insert(*input);
            }
        }
        self.pending.clear();
        for conn in pending {
            self.pending.insert(*conn);
        }
        self.versions.bump_all();
        self.owed = PortSet::new(self.ports.node_count());
        self.fault = None;
        for node in snapshot.in_flight.iter().chain(&snapshot.owed) {
            if let Some(index) = self.ports.node(node) {
                self.owe_host(index);
            }
        }
        self.dirty = true;
    }
}

/// A good default for `max_steps_per_tick`: high enough that only a graph that
/// keeps starting work reaches it.
pub const DEFAULT_MAX_STEPS_PER_TICK: usize = 10_000;

/// Checks a `max_steps_per_tick`: at least 1 ([`RuntimeError::InvalidConfig`]).
pub fn check_max_steps_per_tick(steps: usize) -> Result<(), RuntimeError> {
    if steps == 0 {
        return Err(RuntimeError::InvalidConfig {
            message: "max_steps_per_tick must be at least 1".into(),
        });
    }
    Ok(())
}

/// Checks every node's and stream group's resource claims, so an executor can
/// reject a graph before it instantiates anything.
pub fn check_resources(compiled: &CompiledGraph) -> Result<(), RuntimeError> {
    let ports = PortTable::new(compiled);
    register_resources(compiled, &ports).map(|_| ())
}

/// Each node's claims. Compilation already rejects one claiming more than all of a resource;
/// it could never start, so it stays an error here ([`RuntimeError::InvalidConfig`]).
fn register_resources(
    compiled: &CompiledGraph,
    ports: &PortTable,
) -> Result<ResourcePool, RuntimeError> {
    let graph = compiled.graph();
    let mut resources = ResourcePool::with_capacities(
        graph
            .resources
            .iter()
            .map(|(resource, declared)| (resource.clone(), declared.capacity.get()))
            .collect(),
    );
    for node in &graph.nodes {
        let claims: BTreeMap<ResourceId, f64> = node
            .resources
            .iter()
            .map(|(resource, claim)| (resource.clone(), claim.amount.get()))
            .collect();
        if let Some(at) = ports.node(&node.id) {
            resources
                .register(at.position(), claims)
                .map_err(|resource| RuntimeError::InvalidConfig {
                    message: format!(
                        "node `{}` claims more than all of resource `{resource}`",
                        node.id
                    ),
                })?;
        }
    }
    Ok(resources)
}
