//! The scheduling logic, which holds no values: what to run when, over a
//! [`DataPlane`]. It knows ports by number, through its [`PortTable`], and is
//! told what a `run` changed, never to what.
//!
//! [`SchedulerCore`] ends in its data plane, so that a typed wrapper
//! ([`Scheduler`](crate::Scheduler)) keeps its own plane's type, while
//! everything here is compiled once, for `dyn DataPlane`. See
//! [`scheduler`](crate::scheduler) for the scheduling model.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Waker;

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use witgraph_ir::{
    CompiledGraph, ComponentRef, Connection, ConnectionId, FeedbackIdx, Idx, IndexVec, InputIdx,
    Link, NodeId, NodeIdx, OutputIdx, PortName, PortSet, PortTable, ResourceId,
};

use crate::RuntimeError;
use crate::executor::{DataPlane, LiveIsland, Returned};
use crate::island::{Island, IslandState, Owed, StopCause};
use crate::mode::RuntimeMode;
use crate::node::NodeState;
use crate::plan::{self, IslandPlan, RunShape, Wiring};
use crate::resource::ResourcePool;
use crate::run::{IslandWaker, WakeSet};
use crate::schedule::FaultReport;
use witgraph_traits::PortVersion;

/// Every output port's version, shared: readable while a tick holds the
/// scheduler.
#[derive(Debug, Clone)]
pub(crate) struct PortVersions(Arc<[AtomicU64]>);

impl PortVersions {
    fn new(ports: usize) -> Self {
        Self((0..ports).map(|_| AtomicU64::new(0)).collect())
    }

    /// The version of `output`.
    pub(crate) fn get(&self, output: OutputIdx) -> PortVersion {
        PortVersion::new(self.0[output.position()].load(Ordering::Relaxed))
    }

    pub(crate) fn bump(&self, output: OutputIdx) {
        self.0[output.position()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn bump_all(&self) {
        for version in self.0.iter() {
            version.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A report from an in-flight generation, as the scheduler takes it: what
/// changed, and no values.
#[derive(Debug)]
pub(crate) struct ControlEvent {
    pub(crate) island: usize,
    pub(crate) generation: u64,
    pub(crate) kind: ControlKind,
}

/// What happened in a generation, to the member at `member` in the island
/// plan's order. See [`IslandEventKind`](crate::IslandEventKind).
#[derive(Debug)]
pub(crate) enum ControlKind {
    RunStarted {
        member: usize,
    },
    RunReturned {
        member: usize,
        returned: Returned,
    },
    Rebuilt,
    StreamItems {
        member: usize,
        port: PortName,
        count: usize,
    },
}

/// One island: its static wiring, its lifecycle, and the work it owes.
pub(crate) struct Slot {
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
    pub(crate) state: IslandState<Box<dyn LiveIsland>>,
    pub(crate) owed: Owed,
}

/// The scheduling state of a compiled graph being run, ending in its data
/// plane `P` (see the [module docs](self)).
pub(crate) struct SchedulerCore<P: ?Sized> {
    pub(crate) compiled: CompiledGraph,
    pub(crate) mode: Arc<dyn RuntimeMode>,
    /// Most generations one tick may start.
    pub(crate) max_steps_per_tick: usize,
    /// Islands, in the compiled graph's (topological) island order.
    pub(crate) slots: Vec<Slot>,
    /// How many islands have a generation in flight.
    pub(crate) running: usize,
    /// Each island's index, by its sorted members: how a snapshot names it.
    pub(crate) island_index: HashMap<Vec<NodeId>, usize>,
    /// Every port and feedback connection, numbered.
    pub(crate) ports: Arc<PortTable>,
    /// Each node's island.
    pub(crate) island_of_node: IndexVec<NodeIdx, usize>,
    /// The input ports holding a value. Only Value and handle ports ever
    /// do.
    pub(crate) presence: PortSet<InputIdx>,
    /// The input ports written inside their own generation: a change to
    /// one owes nothing.
    pub(crate) internal: PortSet<InputIdx>,
    /// Each output port's version.
    pub(crate) versions: PortVersions,
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
    /// The feedback connections with a value waiting for the
    /// end-of-iteration latch.
    pub(crate) pending: PortSet<FeedbackIdx>,
    /// The latest fault of each island not yet taken by
    /// [`take_faults`](Self::take_faults).
    pub(crate) faults: BTreeMap<usize, FaultReport>,
    /// Which running islands to poll.
    pub(crate) wakes: Arc<WakeSet>,
    /// Whether anything changed since the last start pass that could let
    /// an island start.
    pub(crate) dirty: bool,
    pub(crate) events_rx: UnboundedReceiver<ControlEvent>,
    pub(crate) resources: ResourcePool,
    /// What holds the values and runs the nodes. Last, so that
    /// `SchedulerCore<P>` coerces to `SchedulerCore<dyn DataPlane>`.
    pub(crate) plane: P,
}

impl SchedulerCore<dyn DataPlane> {
    /// Records a host write that changed an input to a new value.
    pub(crate) fn host_set(&mut self, input: InputIdx) {
        self.presence.insert(input);
        self.owe_for(input);
        self.note_host_write(input);
    }

    /// Records a host write that cleared an input.
    pub(crate) fn host_cleared(&mut self, input: InputIdx) {
        self.presence.remove(input);
        self.owe_for(input);
        self.note_host_write(input);
    }

    /// Makes the node's island owe a run on its latched inputs.
    pub(crate) fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.drain_events();
        let index = self.island(node)?;
        self.slots[index].owed.push_latched();
        self.dirty = true;
        Ok(())
    }

    /// A node's current lifecycle state.
    pub(crate) fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
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

    /// Takes the faults reported since the last call.
    pub(crate) fn take_faults(&mut self) -> Vec<FaultReport> {
        self.drain_events();
        std::mem::take(&mut self.faults).into_values().collect()
    }

    /// Cancels a node's island.
    pub(crate) fn cancel(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        let index = self.island(node)?;
        self.drain_events();
        self.stop_island(index, StopCause::Cancelled);
        Ok(())
    }

    /// Stops everything.
    pub(crate) fn shutdown(&mut self) {
        self.drain_events();
        for index in 0..self.slots.len() {
            self.stop_island(index, StopCause::Shutdown);
        }
        for connection in self.pending.iter() {
            self.plane.drop_feedback(connection);
        }
        self.pending.clear();
    }

    /// The node's island index.
    pub(crate) fn island(&self, node: &NodeId) -> Result<usize, RuntimeError> {
        self.compiled
            .island_of(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
    }
}

impl<P: DataPlane> SchedulerCore<P> {
    /// A scheduler over a compiled graph whose islands are built, with the
    /// data plane `make_plane` makes from the graph's numbered ports, its
    /// islands' plans and the channel for what its generations report.
    pub(crate) fn new(
        compiled: CompiledGraph,
        mode: Arc<dyn RuntimeMode>,
        islands: Vec<(Box<dyn LiveIsland>, Vec<RunShape>)>,
        max_steps_per_tick: usize,
        make_plane: impl FnOnce(
            &CompiledGraph,
            &Arc<PortTable>,
            &[Arc<IslandPlan>],
            UnboundedSender<ControlEvent>,
        ) -> P,
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
        let ports = Arc::new(PortTable::new(&compiled));
        let island_of_node: IndexVec<NodeIdx, usize> = compiled
            .islands()
            .iter()
            .enumerate()
            .flat_map(|(island, members)| members.iter().map(move |_| island))
            .collect();
        let mut internal = PortSet::new(ports.inputs().len());
        let mut feedback_into: Vec<Vec<(FeedbackIdx, usize)>> =
            vec![Vec::new(); ports.inputs().len()];
        let island_of: Vec<usize> = compiled.nodes().map(|n| n.island).collect();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (from, to) = (island_of[resolved.from_node], island_of[resolved.to_node]);
            let Some(input) = ports.input(&conn.to.node, &conn.to.port) else {
                continue;
            };
            match ports.feedback_index(&conn.id) {
                Some(feedback) => feedback_into[input.position()].push((feedback, from)),
                None if from == to => {
                    internal.insert(input);
                }
                None => {}
            }
        }
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
        let plans: Vec<Arc<IslandPlan>> = slots.iter().map(|slot| slot.plan.clone()).collect();
        let (events_tx, events_rx) = mpsc::unbounded();
        let plane = make_plane(&compiled, &ports, &plans, events_tx);
        Ok(Self {
            compiled,
            mode,
            max_steps_per_tick,
            slots,
            running: 0,
            island_index,
            presence: PortSet::new(ports.inputs().len()),
            internal,
            versions: PortVersions::new(ports.outputs().len()),
            pending: PortSet::new(ports.feedback().len()),
            ports,
            island_of_node,
            feedback_into,
            connections,
            components,
            links,
            connected,
            faults: BTreeMap::new(),
            wakes,
            dirty: true,
            events_rx,
            resources,
            plane,
        })
    }
}

/// A good default for [`Scheduler::max_steps_per_tick`](crate::Scheduler::max_steps_per_tick): high enough that
/// only a graph that keeps starting work reaches it.
pub const DEFAULT_MAX_STEPS_PER_TICK: usize = 10_000;

/// Checks a [`Scheduler::max_steps_per_tick`](crate::Scheduler::max_steps_per_tick) as [`Scheduler::new`](crate::Scheduler::new) does:
/// at least 1 ([`RuntimeError::InvalidConfig`]).
pub fn check_max_steps_per_tick(steps: usize) -> Result<(), RuntimeError> {
    if steps == 0 {
        return Err(RuntimeError::InvalidConfig {
            message: "max_steps_per_tick must be at least 1".into(),
        });
    }
    Ok(())
}

/// Checks every island's resource claims as [`Scheduler::new`](crate::Scheduler::new) does, so an
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
impl<P: ?Sized> Drop for SchedulerCore<P> {
    fn drop(&mut self) {
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(generation) = slot.state.running_generation() {
                self.mode.on_generation_stopped(index, generation);
            }
        }
    }
}
