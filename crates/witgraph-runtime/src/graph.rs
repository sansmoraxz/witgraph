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
//! owns the Store), and `Stopped` (faulted, cancelled or shut down; no
//! Store). A node's [`NodePhase`] is a projection of its island's state.
//! What an island still has to run is kept apart from its phase, as a queue
//! of *owed* generations: a run on the latched inputs, or a replay of a
//! restored generation. Invariants:
//!
//! - A newly loaded island owes one run. An external Value input that
//!   changes (`Val ==` decides; writing an equal value is not a change)
//!   makes its island owe a run, whatever phase the island is in. Owed runs
//!   on the latched inputs coalesce: any number of changes before the next
//!   start owe one run.
//! - An island starts its next owed generation only when it is not running;
//!   for a run on the latched inputs, every required external Value input of
//!   every member has a value (a replay brings its own inputs); its resource
//!   claims fit; and no *ancestor* island (one that reaches it over
//!   non-feedback connections) is running or could start. That ordering
//!   keeps a node from running on half-updated inputs. Islands are visited
//!   in order of their shallowest member's depth
//!   (`CompiledGraph::depth_map`). A stopped island is rebuilt into a fresh
//!   Store first.
//! - A generation in flight is never pre-empted: inputs that change while
//!   it runs are owed, and the island re-runs (with fresh stream/future
//!   handles for every member) after it finishes.
//! - A Value output reaching another island is delivered as soon as the
//!   producing `run` returns. A Value output reaching a member of the same
//!   island over a non-feedback connection is part of the generation and
//!   owes nothing.
//! - Feedback connections (Value only) are latched once per tick, at
//!   quiescence: that is one iteration of a loop. A tick that ends early
//!   (`StepLimitReached`, `Aborted`) latches nothing.
//! - A fault (trap, fuel exhaustion, `fatal`) stops the island and faults
//!   every node of it. Owed work survives a fault, so an input that changed
//!   while the failing generation ran makes the island rebuild and run
//!   again; otherwise a faulted island waits for its next input change. If
//!   the rebuild itself fails, every member faults with
//!   [`NodeFault::Restart`] and the owed work is dropped.
//! - Cancellation and shutdown drop the generation and the Store and forget
//!   owed work; the island is rebuilt on its next input change.
//!
//! In-flight generations live in the runtime graph, not in the future
//! returned by [`tick`](RuntimeGraph::tick): dropping a tick (on a
//! timeout, say) loses nothing, and the next tick resumes them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;

use futures::FutureExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::stream::StreamExt;
use wasmtime::Engine;
use wasmtime::component::{Component, Linker, Type, Val};
use witgraph_ir::{
    CompiledGraph, ComponentContract, ComponentRef, ConnectionId, NodeId, NodeShape, PortKind,
    PortName, PortRef, ResourceId,
};

use crate::engine::{
    self, FuelPolicy, HostState, InputField, InputSource, IslandEvent, IslandEventKind, IslandPlan,
    MemberPlan, Outcome, OutputField, RunSignature,
};
use crate::error::{NodeFault, RuntimeError};
use crate::island::{self, Island, IslandState, Owed, OwedQueue, StartInputs, StopCause};
use crate::mode::{Release, RuntimeMode};
use crate::node::{NodePhase, NodeState};
use crate::resource::ResourcePool;
use crate::schedule::TickResult;

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
pub struct Snapshot {
    /// Each node's resolved component (id and content hash). A snapshot
    /// restores only onto a graph with exactly these nodes and components.
    pub nodes: BTreeMap<NodeId, ComponentRef>,
    /// Whether no generation was in flight when the snapshot was taken.
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
    /// Feedback values waiting for the next latch, by connection.
    pub feedback: BTreeMap<ConnectionId, String>,
    /// Islands with an in-flight or queued generation.
    pub islands: Vec<IslandSnapshot>,
}

/// An island with work outstanding when a [`Snapshot`] was taken.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IslandSnapshot {
    /// The island's members, in the compiled graph's island order.
    pub members: Vec<NodeId>,
    /// The external Value inputs the in-flight generation started with, by
    /// member; `None` when no generation was in flight.
    pub running: Option<PortValues>,
    /// Whether another generation was queued (an input changed since the
    /// last start). It runs with the latched inputs.
    pub queued: bool,
}

/// Configuration for a runtime graph.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum generations started per tick (a safety limit against value
    /// cycles that never settle).
    pub max_steps_per_tick: usize,
    /// Fuel an island burns before yielding to the executor, so one busy
    /// island cannot starve the others. `None` disables yielding.
    pub yield_interval: Option<u64>,
    /// Fuel an island may burn between two `run` starts before it faults
    /// with [`NodeFault::FuelExhausted`]: the island's fuel is reset to this
    /// budget before every `run` call. The budget is shared by everything
    /// executing in the island, spawned tasks included, so an endless
    /// streaming generation exhausts any finite budget eventually. `None`
    /// means effectively unlimited.
    pub fuel_per_run: Option<u64>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps_per_tick: 10_000,
            yield_interval: Some(100_000),
            fuel_per_run: None,
        }
    }
}

/// An outgoing connection of a Value output port.
#[derive(Debug, Clone)]
struct Edge {
    id: ConnectionId,
    to: PortRef,
    feedback: bool,
}

/// One island: its static wiring, its lifecycle, and the work it owes.
struct Slot {
    plan: Arc<IslandPlan>,
    /// Islands that must settle before this one starts.
    ancestors: Vec<usize>,
    state: IslandState,
    owed: OwedQueue,
}

/// What the tick loop woke up for.
enum Wake {
    Event(IslandEvent),
    Finished(usize, Outcome),
}

/// A compiled graph loaded with WASM components, ready for execution.
pub struct RuntimeGraph<M: RuntimeMode = Release> {
    compiled: CompiledGraph,
    config: RuntimeConfig,
    mode: M,
    engine: Engine,
    components: HashMap<NodeId, Component>,
    linkers: HashMap<NodeId, Linker<HostState>>,
    slots: Vec<Slot>,
    start_order: Vec<usize>,
    shapes: HashMap<NodeId, NodeShape>,
    node_island: HashMap<NodeId, usize>,
    /// Latched value of every Value input port that has one.
    inputs: HashMap<PortRef, Val>,
    /// Payload type of every Value input port (inner type when optional).
    input_types: HashMap<PortRef, Type>,
    /// Type of every Value output port.
    output_types: HashMap<PortRef, Type>,
    /// Latched Value outputs.
    outputs: HashMap<PortRef, Val>,
    out_edges: HashMap<PortRef, Vec<Edge>>,
    /// Feedback writes waiting for the end-of-tick latch.
    feedback: BTreeMap<ConnectionId, (PortRef, Val)>,
    events_tx: UnboundedSender<IslandEvent>,
    events_rx: UnboundedReceiver<IslandEvent>,
    resources: ResourcePool,
    pending_abort: Option<TickResult>,
    step_limit_hit: bool,
}

impl<M: RuntimeMode> RuntimeGraph<M> {
    /// Loads a compiled graph with WASM component bytes.
    ///
    /// `wasm` maps each component (by the [`ComponentRef`] of its resolved
    /// contract, or the same ref without a content hash) to its encoded
    /// component bytes. Each component's embedded WIT is decoded and
    /// lowered, and must hash to the contract the graph was compiled
    /// against. Every island is instantiated.
    pub async fn load(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
    ) -> Result<Self, RuntimeError> {
        Self::load_with_linker(compiled, wasm, config, mode, |_| Ok(())).await
    }

    /// Like [`load`](Self::load), but lets the embedder add capability
    /// imports to each node's linker.
    ///
    /// The callback runs once per node, after the built-in
    /// `witgraph:runtime/host` interface has been added. Linkers are kept
    /// and reused when an island is rebuilt after a fault or cancellation.
    pub async fn load_with_linker<F>(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
        configure_linker: F,
    ) -> Result<Self, RuntimeError>
    where
        F: Fn(&mut Linker<HostState>) -> wasmtime::Result<()>,
    {
        validate(&config)?;
        let engine = engine::new_engine().map_err(|e| RuntimeError::InvalidConfig {
            message: format!("{e:#}"),
        })?;

        // Verify and compile every component, once.
        let mut compiled_components: HashMap<ComponentRef, Component> = HashMap::new();
        for contract in compiled.contracts() {
            let bytes =
                find_bytes(wasm, &contract.id).ok_or_else(|| RuntimeError::MissingWasm {
                    component: Box::new(contract.id.clone()),
                })?;
            verify(contract, bytes)?;
            let component =
                Component::new(&engine, bytes).map_err(|e| RuntimeError::BadComponent {
                    component: Box::new(contract.id.clone()),
                    message: format!("{e:#}"),
                })?;
            compiled_components.insert(contract.id.clone(), component);
        }

        let graph = compiled.graph();
        let mut components = HashMap::new();
        let mut linkers = HashMap::new();
        for node in &graph.nodes {
            let contract = contract_of(&compiled, &node.id)?;
            let component = compiled_components
                .get(&contract.id)
                .cloned()
                .ok_or_else(|| RuntimeError::MissingWasm {
                    component: Box::new(contract.id.clone()),
                })?;
            let linker =
                engine::node_linker(&engine, &node.id, &configure_linker).map_err(|e| {
                    RuntimeError::Instantiation {
                        node: node.id.clone(),
                        message: format!("linker: {e:#}"),
                    }
                })?;
            components.insert(node.id.clone(), component);
            linkers.insert(node.id.clone(), linker);
        }

        let mut out_edges: HashMap<PortRef, Vec<Edge>> = HashMap::new();
        for conn in &graph.connections {
            out_edges.entry(conn.from.clone()).or_default().push(Edge {
                id: conn.id.clone(),
                to: conn.to.clone(),
                feedback: conn.feedback,
            });
        }

        let fuel = FuelPolicy {
            yield_interval: config.yield_interval,
            per_run: config.fuel_per_run,
        };
        let mut node_island = HashMap::new();
        let mut islands = Vec::new();
        let mut input_types = HashMap::new();
        let mut output_types = HashMap::new();
        for (index, members) in compiled.islands().iter().enumerate() {
            for node in members {
                node_island.insert(node.clone(), index);
            }
            let parts: Vec<_> = members
                .iter()
                .map(|node| (node, &components[node], &linkers[node]))
                .collect();
            let (island, signatures) = engine::build_island(&engine, &parts, fuel)
                .await
                .map_err(|(node, message)| RuntimeError::Instantiation { node, message })?;
            let plan = plan_island(
                &compiled,
                index,
                members,
                &signatures,
                &mut input_types,
                &mut output_types,
            )?;
            islands.push((plan, island));
        }

        // Resource claims, summed per island.
        let mut resources = ResourcePool::default();
        for (index, members) in compiled.islands().iter().enumerate() {
            let mut claims: BTreeMap<ResourceId, f64> = BTreeMap::new();
            for node in members {
                if let Some(n) = graph.nodes.iter().find(|n| &n.id == node) {
                    for (resource, claim) in &n.resources {
                        *claims.entry(resource.clone()).or_insert(0.0) += claim.fraction.get();
                    }
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

        let ancestors = island_ancestors(&compiled, &node_island);
        let depth = compiled.depth_map();
        let mut start_order: Vec<usize> = (0..islands.len()).collect();
        start_order.sort_by_key(|&i| {
            let shallowest = compiled.islands()[i]
                .iter()
                .filter_map(|n| depth.get(n).copied())
                .min()
                .unwrap_or(0);
            (shallowest, i)
        });

        let slots = islands
            .into_iter()
            .zip(ancestors)
            .map(|((plan, store), ancestors)| {
                // A new island owes its first generation.
                let mut owed = OwedQueue::default();
                owed.push_latched();
                Slot {
                    plan: Arc::new(plan),
                    ancestors,
                    state: Island::new(store).into(),
                    owed,
                }
            })
            .collect();

        let shapes = graph
            .nodes
            .iter()
            .filter_map(|node| Some((node.id.clone(), compiled.contract_for(&node.id)?.shape())))
            .collect();

        let (events_tx, events_rx) = mpsc::unbounded();
        Ok(Self {
            compiled,
            config,
            mode,
            engine,
            components,
            linkers,
            slots,
            start_order,
            shapes,
            node_island,
            inputs: HashMap::new(),
            input_types,
            output_types,
            outputs: HashMap::new(),
            out_edges,
            feedback: BTreeMap::new(),
            events_tx,
            events_rx,
            resources,
            pending_abort: None,
            step_limit_hit: false,
        })
    }

    /// Writes a value to a Value input port, as an external source would.
    ///
    /// The value must have the port's payload type (for an optional port,
    /// the inner type). Writing a value equal to the current one is not a
    /// change; otherwise the node's island re-runs on the next tick (and a
    /// faulted or cancelled island is rebuilt first).
    pub fn inject(&mut self, node: NodeId, port: PortName, val: Val) -> Result<(), RuntimeError> {
        if !self.node_island.contains_key(&node) {
            return Err(RuntimeError::UnknownNode { node });
        }
        let port_ref = PortRef::new(node.clone(), port.clone());
        let Some(ty) = self.input_types.get(&port_ref) else {
            return Err(RuntimeError::NotAValuePort {
                node,
                port,
                direction: "input",
            });
        };
        engine::check_type(ty, &val).map_err(|message| RuntimeError::ValueType {
            node: node.clone(),
            port: port.clone(),
            message,
        })?;
        self.set_input(port_ref, val);
        Ok(())
    }

    /// The latched value of a Value output port: what the node's `run`
    /// returned last. `None` if the node has not run yet, or the port is
    /// not a Value output.
    pub fn read_output(&self, node: &NodeId, port: &PortName) -> Option<Val> {
        self.outputs
            .get(&PortRef::new(node.clone(), port.clone()))
            .cloned()
    }

    /// A node's current lifecycle state: a view of its island's state at
    /// this moment (see [`crate::node`]). `None` for an unknown node.
    pub fn node_state(&self, node: &NodeId) -> Option<NodeState> {
        let state = &self.slots[*self.node_island.get(node)?].state;
        Some(NodeState::new(
            node.clone(),
            self.shapes.get(node).copied()?,
            state.node_phase(),
            state.fault().cloned(),
        ))
    }

    /// Captures the graph's host-visible state, at any time: latched Value
    /// inputs and outputs, pending feedback, every node's phase, and the
    /// start inputs of each in-flight or queued generation. See
    /// [`Snapshot`] for what is and is not captured.
    ///
    /// Snapshots do not store stream or future contents. Restoring re-runs any
    /// generation that was in flight, which recreates its streams from the
    /// recorded inputs; the result matches the original only if the guests are
    /// deterministic.
    ///
    /// Fails only if a latched value cannot be rendered as WAVE text, which
    /// a Value port never holds.
    pub fn snapshot(&self) -> Result<Snapshot, RuntimeError> {
        let wave = |node: &NodeId, port: &PortName, val: &Val| {
            val.to_wave().map_err(|e| RuntimeError::SnapshotValue {
                node: node.clone(),
                port: port.clone(),
                message: format!("{e:#}"),
            })
        };
        let values = |map: &HashMap<PortRef, Val>| -> Result<PortValues, RuntimeError> {
            let mut out = PortValues::new();
            for (port, val) in map {
                out.entry(port.node.clone())
                    .or_default()
                    .insert(port.port.clone(), wave(&port.node, &port.port, val)?);
            }
            Ok(out)
        };
        let mut feedback = BTreeMap::new();
        for (conn, (to, val)) in &self.feedback {
            feedback.insert(conn.clone(), wave(&to.node, &to.port, val)?);
        }
        let mut islands = Vec::new();
        for slot in &self.slots {
            // In flight, or a restored replay not started yet.
            let running = match slot.state.started_with().or(slot.owed.replay()) {
                Some(started) => {
                    let mut by_member = PortValues::new();
                    for (member, inputs) in slot.plan.members.iter().zip(started) {
                        let entry = by_member.entry(member.node.clone()).or_default();
                        for (port, val) in inputs {
                            entry.insert(port.clone(), wave(&member.node, port, val)?);
                        }
                    }
                    Some(by_member)
                }
                None => None,
            };
            let queued = slot.owed.has_latched();
            if running.is_some() || queued {
                islands.push(IslandSnapshot {
                    members: slot.plan.members.iter().map(|m| m.node.clone()).collect(),
                    running,
                    queued,
                });
            }
        }
        Ok(Snapshot {
            nodes: self.node_components(),
            quiescent: !self.any_running(),
            phases: self
                .node_island
                .iter()
                .map(|(id, &index)| (id.clone(), self.slots[index].state.node_phase()))
                .collect(),
            inputs: values(&self.inputs)?,
            outputs: values(&self.outputs)?,
            feedback,
            islands,
        })
    }

    /// Replaces the graph's host-visible state with `snapshot`. The next
    /// [`tick`](Self::tick) *replays* every generation that was in flight
    /// when the snapshot was taken, from its start and with the inputs it
    /// started with, and runs queued generations as usual.
    ///
    /// Snapshots do not store stream or future contents. Restoring re-runs any
    /// generation that was in flight, which recreates its streams from the
    /// recorded inputs; the result matches the original only if the guests are
    /// deterministic.
    ///
    /// The graph must have nothing in flight — freshly loaded, after
    /// [`shutdown`](Self::shutdown), or right after every running island
    /// was [`cancel`](Self::cancel)led — and exactly the snapshot's nodes and
    /// components (by content hash). Every value is parsed against its
    /// port's type; on any error nothing is changed. Node phases are not
    /// restored: a stopped island is rebuilt when its replay or next
    /// generation starts. A replay runs before its own island's queued run,
    /// but like any start it waits for the island's ancestors and resources,
    /// so other islands may start first.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        if self.any_running() {
            return Err(RuntimeError::NotQuiescent);
        }
        let ours = self.node_components();
        if snapshot.nodes != ours {
            let differs = ours
                .keys()
                .chain(snapshot.nodes.keys())
                .find(|n| ours.get(*n) != snapshot.nodes.get(*n))
                .map_or_else(|| "?".into(), |n| format!("node `{n}`"));
            return Err(RuntimeError::SnapshotMismatch {
                message: format!("{differs} differs (missing, extra, or another component)"),
            });
        }
        let parse = |types: &HashMap<PortRef, Type>, port: &PortRef, text: &str, what: &str| {
            let invalid = |message: String| RuntimeError::SnapshotValue {
                node: port.node.clone(),
                port: port.port.clone(),
                message,
            };
            let ty = types
                .get(port)
                .ok_or_else(|| invalid(format!("not a Value {what} port")))?;
            engine::parse_wave(ty, text).map_err(invalid)
        };
        let parse_values = |values: &PortValues,
                            types: &HashMap<PortRef, Type>,
                            what: &str|
         -> Result<HashMap<PortRef, Val>, RuntimeError> {
            let mut out = HashMap::new();
            for (node, ports) in values {
                for (port, text) in ports {
                    let port = PortRef::new(node.clone(), port.clone());
                    let val = parse(types, &port, text, what)?;
                    out.insert(port, val);
                }
            }
            Ok(out)
        };
        let inputs = parse_values(&snapshot.inputs, &self.input_types, "input")?;
        let outputs = parse_values(&snapshot.outputs, &self.output_types, "output")?;
        let mut feedback = BTreeMap::new();
        for (conn, text) in &snapshot.feedback {
            let to = self
                .compiled
                .graph()
                .connections
                .iter()
                .find(|c| c.id == *conn && c.feedback)
                .map(|c| c.to.clone())
                .ok_or_else(|| RuntimeError::SnapshotMismatch {
                    message: format!("`{conn}` is not a feedback connection of this graph"),
                })?;
            let val = parse(&self.input_types, &to, text, "input")?;
            feedback.insert(conn.clone(), (to, val));
        }
        let mut work: HashMap<usize, (Option<StartInputs>, bool)> = HashMap::new();
        for island in &snapshot.islands {
            let index = island
                .members
                .first()
                .and_then(|n| self.node_island.get(n).copied())
                .filter(|&i| self.member_ids(i) == island.members)
                .ok_or_else(|| RuntimeError::SnapshotMismatch {
                    message: format!("no island with members {:?}", island.members),
                })?;
            let replay = match &island.running {
                None => None,
                Some(by_member) => {
                    let plan = &self.slots[index].plan;
                    let mut external = vec![HashMap::new(); plan.members.len()];
                    for (node, ports) in by_member {
                        let position = plan
                            .members
                            .iter()
                            .position(|m| &m.node == node)
                            .ok_or_else(|| RuntimeError::SnapshotMismatch {
                                message: format!("`{node}` is not in its island"),
                            })?;
                        for (port, text) in ports {
                            let port_ref = PortRef::new(node.clone(), port.clone());
                            let val = parse(&self.input_types, &port_ref, text, "input")?;
                            external[position].insert(port.clone(), val);
                        }
                    }
                    Some(external)
                }
            };
            work.insert(index, (replay, island.queued));
        }

        self.inputs = inputs;
        self.outputs = outputs;
        self.feedback = feedback;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let (replay, queued) = work.remove(&index).unwrap_or((None, false));
            slot.owed.clear();
            if let Some(inputs) = replay {
                slot.owed.push_replay(inputs);
            }
            if queued {
                slot.owed.push_latched();
            }
        }
        Ok(())
    }

    /// Runs the graph until it is quiescent, then latches feedback
    /// connections.
    ///
    /// Starts every island that can start, drives all in-flight
    /// generations concurrently, delivers Value outputs as `run`s return,
    /// and keeps starting islands whose inputs changed, until nothing is in
    /// flight and nothing can start. Then each feedback connection's latest
    /// value is written to its target (one loop iteration). A tick that
    /// returns [`TickResult::StepLimitReached`] or [`TickResult::Aborted`]
    /// returns before that latch, so it latches nothing.
    ///
    /// An island whose generation never finishes (an endless stream, say)
    /// keeps the tick running; bound it with the executor (a timeout or a
    /// `select`) or [`cancel`](Self::cancel) the node. Dropping the tick's
    /// future is safe: in-flight generations persist and resume on the
    /// next tick.
    pub async fn tick(&mut self) -> TickResult {
        let mut progressed = false;
        let mut steps = 0usize;
        self.step_limit_hit = false;
        loop {
            progressed |= self.drain_events();
            if let Some(abort) = self.pending_abort.take() {
                return abort;
            }
            progressed |= self.start_ready(&mut steps).await;
            if self.step_limit_hit {
                return TickResult::StepLimitReached;
            }
            if !self.any_running() {
                break;
            }
            progressed |= match self.next_wake().await {
                Wake::Event(event) => self.handle_event(event),
                Wake::Finished(index, outcome) => {
                    // Events sent before the generation finished come first.
                    self.drain_events();
                    self.handle_finished(index, outcome);
                    true
                }
            };
            if let Some(abort) = self.pending_abort.take() {
                return abort;
            }
        }
        progressed |= self.latch_feedback();
        if progressed {
            TickResult::Progress
        } else {
            TickResult::Idle
        }
    }

    /// Cancels a node's island: an in-flight generation is dropped (and
    /// with it the island's Store), any work the island owed is forgotten,
    /// and every node of the island becomes
    /// [`Cancelled`](NodePhase::Cancelled). A faulted island becomes
    /// cancelled too; cancelling an island that is already cancelled does
    /// nothing. The island is rebuilt when an input next changes.
    pub fn cancel(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        let index = *self
            .node_island
            .get(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        if matches!(
            self.slots[index].state.stop_cause(),
            Some(StopCause::Cancelled | StopCause::Shutdown)
        ) {
            return Ok(());
        }
        self.stop_island(index, StopCause::Cancelled);
        Ok(())
    }

    /// Stops everything: drops every in-flight generation and every Store,
    /// forgets all owed work, and cancels every island that is still live.
    /// A faulted island keeps its fault. A later input change rebuilds the
    /// affected island as after [`cancel`](Self::cancel).
    pub async fn shutdown(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].state.is_stopped() {
                self.slots[index].owed.clear();
            } else {
                self.stop_island(index, StopCause::Shutdown);
            }
        }
    }

    /// The compiled graph this runtime was loaded from.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// The runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// The runtime mode (for [`struct@crate::Debug`], its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// The wasmtime engine every island runs on.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    // ------------------------------------------------------------------
    // Scheduler internals
    // ------------------------------------------------------------------

    /// Each node's resolved component reference.
    fn node_components(&self) -> BTreeMap<NodeId, ComponentRef> {
        self.compiled
            .graph()
            .nodes
            .iter()
            .filter_map(|n| Some((n.id.clone(), self.compiled.contract_for(&n.id)?.id.clone())))
            .collect()
    }

    fn member_ids(&self, index: usize) -> Vec<NodeId> {
        self.slots[index]
            .plan
            .members
            .iter()
            .map(|m| m.node.clone())
            .collect()
    }

    /// Whether any island has a generation in flight.
    fn any_running(&self) -> bool {
        self.slots.iter().any(|slot| slot.state.is_running())
    }

    fn fuel_policy(&self) -> FuelPolicy {
        FuelPolicy {
            yield_interval: self.config.yield_interval,
            per_run: self.config.fuel_per_run,
        }
    }

    /// Applies an island transition and reports the resulting node-phase
    /// change for every member.
    fn transition(&mut self, index: usize, step: impl FnOnce(IslandState) -> IslandState) {
        let state = std::mem::replace(&mut self.slots[index].state, IslandState::placeholder());
        let from = state.node_phase();
        let next = step(state);
        let to = next.node_phase();
        self.slots[index].state = next;
        if from != to {
            for member in self.member_ids(index) {
                self.mode.on_phase_transition(&member, from, to);
            }
        }
    }

    /// Writes a Value input from outside its island. Returns whether the
    /// value changed (and so made the island owe a generation).
    fn set_input(&mut self, port: PortRef, val: Val) -> bool {
        if self.inputs.get(&port) == Some(&val) {
            return false;
        }
        if let Some(&index) = self.node_island.get(&port.node) {
            self.slots[index].owed.push_latched();
        }
        self.inputs.insert(port, val);
        true
    }

    fn inputs_ready(&self, index: usize) -> bool {
        self.slots[index].plan.members.iter().all(|member| {
            member.inputs.iter().flatten().all(|field| {
                field.optional
                    || field.kind != PortKind::Value
                    || field.source == InputSource::Member
                    || self
                        .inputs
                        .contains_key(&PortRef::new(member.node.clone(), field.name.clone()))
            })
        })
    }

    /// Whether the island could start its next owed generation, ignoring
    /// its ancestors and resources. A replay brings its own inputs; a run on
    /// the latched inputs needs every required one to have a value.
    fn could_start(&self, index: usize) -> bool {
        let slot = &self.slots[index];
        !slot.state.is_running()
            && match slot.owed.front() {
                None => false,
                Some(Owed::Replay(_)) => true,
                Some(Owed::Latched) => self.inputs_ready(index),
            }
    }

    fn is_startable(&self, index: usize) -> bool {
        self.could_start(index)
            && self.slots[index]
                .ancestors
                .iter()
                .all(|&a| !self.slots[a].state.is_running() && !self.could_start(a))
    }

    /// Starts every island that can start, in depth order. Returns whether
    /// any started.
    async fn start_ready(&mut self, steps: &mut usize) -> bool {
        let mut started = false;
        for position in 0..self.start_order.len() {
            let index = self.start_order[position];
            if !self.is_startable(index) || !self.resources.can_acquire(index) {
                continue;
            }
            if *steps >= self.config.max_steps_per_tick {
                self.step_limit_hit = true;
                return started;
            }
            *steps += 1;
            started = true;
            self.start_generation(index).await;
        }
        started
    }

    /// Starts the island's next owed generation, rebuilding it first if it
    /// is stopped.
    async fn start_generation(&mut self, index: usize) {
        if self.slots[index].state.is_stopped() && !self.rebuild(index).await {
            return;
        }
        let Some(work) = self.slots[index].owed.pop_front() else {
            return;
        };
        let external = match work {
            Owed::Replay(inputs) => inputs,
            Owed::Latched => self.external_inputs(index),
        };
        let plan = self.slots[index].plan.clone();
        let fuel = self.fuel_policy();
        let events = self.events_tx.clone();
        let started_with = external.clone();
        let mut generation = 0;
        self.resources.acquire(index);
        self.transition(index, |state| match state {
            IslandState::Idle(island) => island
                .start(started_with, |store, number| {
                    generation = number;
                    engine::run_generation(store, plan, external, fuel, number, events).boxed()
                })
                .into(),
            other => island::misuse(other, "Idle"),
        });
        self.mode.on_generation_started(index, generation);
    }

    /// Rebuilds a stopped island into a fresh Store. Returns whether it
    /// succeeded. On failure every member faults with
    /// [`NodeFault::Restart`] and the island's owed work is forgotten, so a
    /// broken component is not rebuilt over and over.
    async fn rebuild(&mut self, index: usize) -> bool {
        let members = self.member_ids(index);
        let parts: Vec<_> = members
            .iter()
            .filter_map(|node| Some((node, self.components.get(node)?, self.linkers.get(node)?)))
            .collect();
        let built = engine::build_island(&self.engine, &parts, self.fuel_policy()).await;
        match built {
            Ok((store, _)) => {
                self.transition(index, |state| state.rebuild(store));
                for member in &members {
                    self.mode.on_restarted(member);
                }
                true
            }
            Err((_, message)) => {
                self.slots[index].owed.clear();
                self.fault_island(index, NodeFault::Restart { message });
                false
            }
        }
    }

    /// The external Value inputs of each member, in plan order.
    fn external_inputs(&self, index: usize) -> StartInputs {
        self.slots[index]
            .plan
            .members
            .iter()
            .map(|member| {
                member
                    .inputs
                    .iter()
                    .flatten()
                    .filter(|f| f.source == InputSource::External && f.kind == PortKind::Value)
                    .filter_map(|f| {
                        let val = self
                            .inputs
                            .get(&PortRef::new(member.node.clone(), f.name.clone()))?;
                        Some((f.name.clone(), val.clone()))
                    })
                    .collect()
            })
            .collect()
    }

    /// Waits for the next island event or finished generation. Every
    /// running island's generation is polled here; a generation lives in its
    /// island's state, so dropping this future loses nothing.
    async fn next_wake(&mut self) -> Wake {
        let Self {
            slots, events_rx, ..
        } = self;
        poll_fn(|cx| {
            if let Poll::Ready(Some(event)) = events_rx.poll_next_unpin(cx) {
                return Poll::Ready(Wake::Event(event));
            }
            for (index, slot) in slots.iter_mut().enumerate() {
                if let IslandState::Running(island) = &mut slot.state
                    && let Poll::Ready(outcome) = island.poll(cx)
                {
                    return Poll::Ready(Wake::Finished(index, outcome));
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Handles every event already queued. Returns whether any belonged
    /// to a current generation.
    fn drain_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.events_rx.try_recv() {
            any |= self.handle_event(event);
        }
        any
    }

    /// Handles one event. Returns `false` for a stale one (from a dropped
    /// generation).
    fn handle_event(&mut self, event: IslandEvent) -> bool {
        let current = self
            .slots
            .get(event.island)
            .and_then(|slot| slot.state.running_generation());
        if current != Some(event.generation) {
            return false;
        }
        match event.kind {
            IslandEventKind::RunStarted { node } => self.mode.on_run_started(&node),
            IslandEventKind::RunReturned { node, values } => {
                self.mode.on_run_returned(&node);
                self.deliver(&node, values);
            }
        }
        true
    }

    /// Latches a returned `run`'s Value outputs and routes them.
    fn deliver(&mut self, node: &NodeId, values: Vec<(PortName, Val)>) {
        let source = self.node_island.get(node).copied();
        for (port, val) in values {
            let output = PortRef::new(node.clone(), port);
            for edge in self.out_edges.get(&output).cloned().unwrap_or_default() {
                if edge.feedback {
                    self.feedback.insert(edge.id, (edge.to, val.clone()));
                } else if self.node_island.get(&edge.to.node).copied() == source {
                    // Consumed inside this generation; recorded, not a change.
                    self.inputs.insert(edge.to, val.clone());
                } else {
                    self.set_input(edge.to, val.clone());
                }
            }
            self.outputs.insert(output, val);
        }
    }

    /// Handles a running island's finished generation. Work the island
    /// owes survives a fault: if an input changed while the failing
    /// generation ran, the island is rebuilt and runs again.
    fn handle_finished(&mut self, index: usize, outcome: Outcome) {
        let generation = self.slots[index].state.running_generation().unwrap_or(0);
        self.resources.release(index);
        match outcome {
            Ok(store) => {
                self.transition(index, |state| state.finish(store));
                self.mode.on_generation_finished(index, generation);
            }
            Err((fault, caller)) => {
                if let NodeFault::Fatal { .. } = fault {
                    let node = caller
                        .or_else(|| self.member_ids(index).into_iter().next())
                        .unwrap_or_else(|| NodeId::from("?"));
                    self.pending_abort = Some(TickResult::Aborted {
                        node,
                        fault: fault.clone(),
                    });
                }
                self.fault_island(index, fault);
            }
        }
    }

    /// Stops the island with a fault and reports it for every member.
    fn fault_island(&mut self, index: usize, fault: NodeFault) {
        self.transition(index, |state| state.stop(StopCause::Faulted(fault.clone())));
        for member in self.member_ids(index) {
            self.mode.on_node_fault(&member, &fault);
        }
    }

    /// Stops a live island (or re-stops a faulted one) because of the host:
    /// drops its generation and Store, forgets its owed work, releases its
    /// resources, and reports every member cancelled.
    fn stop_island(&mut self, index: usize, cause: StopCause) {
        let was_running = self.slots[index].state.is_running();
        self.slots[index].owed.clear();
        self.transition(index, |state| state.stop(cause));
        if was_running {
            self.resources.release(index);
        }
        for member in self.member_ids(index) {
            self.mode.on_cancelled(&member);
        }
    }

    /// Writes buffered feedback values to their targets. Returns whether
    /// any changed.
    fn latch_feedback(&mut self) -> bool {
        let mut changed = false;
        for (_, (to, val)) in std::mem::take(&mut self.feedback) {
            changed |= self.set_input(to, val);
        }
        changed
    }
}

fn validate(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    let invalid = |message: &str| {
        Err(RuntimeError::InvalidConfig {
            message: message.into(),
        })
    };
    if config.max_steps_per_tick == 0 {
        return invalid("max_steps_per_tick must be at least 1");
    }
    if config.yield_interval == Some(0) {
        return invalid("yield_interval must be at least 1");
    }
    if config.fuel_per_run == Some(0) {
        return invalid("fuel_per_run must be at least 1");
    }
    Ok(())
}

fn contract_of<'a>(
    compiled: &'a CompiledGraph,
    node: &NodeId,
) -> Result<&'a ComponentContract, RuntimeError> {
    compiled
        .contract_for(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
}

/// The bytes for a component: keyed by its full ref, or by the same ref
/// without a content hash.
fn find_bytes<'a>(wasm: &HashMap<ComponentRef, &'a [u8]>, id: &ComponentRef) -> Option<&'a [u8]> {
    wasm.get(id).copied().or_else(|| {
        wasm.iter()
            .find(|(key, _)| {
                key.content_hash.is_none() && key.package == id.package && key.world == id.world
            })
            .map(|(_, bytes)| *bytes)
    })
}

/// Decodes the WIT embedded in a component, lowers it, and checks it hashes
/// to the contract the graph was compiled against.
fn verify(contract: &ComponentContract, bytes: &[u8]) -> Result<(), RuntimeError> {
    let bad = |message: String| RuntimeError::BadComponent {
        component: Box::new(contract.id.clone()),
        message,
    };
    let decoded = wit_parser::decoding::decode(bytes).map_err(|e| bad(format!("{e:#}")))?;
    let package = decoded.package();
    let wit_parser::decoding::DecodedWasm::Component(resolve, _) = decoded else {
        return Err(bad("the bytes are a WIT package, not a component".into()));
    };
    let source = witgraph_wit::load::WitSource {
        resolve,
        packages: vec![package],
    };
    let lowered = witgraph_wit::lower::lower(&source).map_err(|e| bad(e.to_string()))?;
    let [found] = lowered.as_slice() else {
        return Err(bad(format!(
            "expected exactly one node world, found {}",
            lowered.len()
        )));
    };
    let expected = witgraph_wit::hash::content_hash(contract);
    let found = witgraph_wit::hash::content_hash(&found.contract);
    if expected != found {
        return Err(RuntimeError::ContractMismatch {
            component: Box::new(contract.id.clone()),
            expected,
            found,
        });
    }
    Ok(())
}

/// Computes an island's static wiring from its contracts and the run
/// signatures of its instantiated members, recording Value port types.
fn plan_island(
    compiled: &CompiledGraph,
    index: usize,
    members: &[NodeId],
    signatures: &[RunSignature],
    input_types: &mut HashMap<PortRef, Type>,
    output_types: &mut HashMap<PortRef, Type>,
) -> Result<IslandPlan, RuntimeError> {
    let position: HashMap<&NodeId, usize> =
        members.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let connections = &compiled.graph().connections;
    let mut plans = Vec::with_capacity(members.len());
    for (i, node) in members.iter().enumerate() {
        let contract = contract_of(compiled, node)?;
        let signature = &signatures[i];
        let internal_producer = |port: &str| {
            connections.iter().find_map(|c| {
                (!c.feedback && c.to.node == *node && c.to.port.as_str() == port)
                    .then(|| position.get(&c.from.node).copied())
                    .flatten()
            })
        };

        let mut deps = BTreeSet::new();
        let inputs = signature.inputs.as_ref().map(|fields| {
            fields
                .iter()
                .filter_map(|(name, ty)| {
                    let port = contract.inputs.iter().find(|p| p.name.as_str() == name)?;
                    let producer = internal_producer(name);
                    if let Some(p) = producer {
                        deps.insert(p);
                    }
                    if port.kind == PortKind::Value {
                        let payload = match (port.optional, ty) {
                            (true, Type::Option(option)) => option.ty(),
                            _ => ty.clone(),
                        };
                        input_types.insert(PortRef::new(node.clone(), port.name.clone()), payload);
                    }
                    Some(InputField {
                        name: port.name.clone(),
                        kind: port.kind,
                        optional: port.optional,
                        source: if producer.is_some() {
                            InputSource::Member
                        } else {
                            InputSource::External
                        },
                    })
                })
                .collect()
        });

        for (name, ty) in &signature.outputs {
            if let Some(port) = contract
                .outputs
                .iter()
                .find(|p| p.name.as_str() == name && p.kind == PortKind::Value)
            {
                output_types.insert(PortRef::new(node.clone(), port.name.clone()), ty.clone());
            }
        }

        let outputs = contract
            .outputs
            .iter()
            .map(|port| {
                let consumers = connections
                    .iter()
                    .filter(|c| !c.feedback && c.from.node == *node && c.from.port == port.name)
                    .filter_map(|c| Some((*position.get(&c.to.node)?, c.to.port.clone())))
                    .collect();
                (
                    port.name.clone(),
                    OutputField {
                        kind: port.kind,
                        consumers,
                    },
                )
            })
            .collect();

        plans.push(MemberPlan {
            node: node.clone(),
            inputs,
            has_result: signature.has_result,
            outputs,
            deps: deps.into_iter().collect(),
            dependents: Vec::new(),
        });
    }
    for i in 0..plans.len() {
        for d in plans[i].deps.clone() {
            plans[d].dependents.push(i);
        }
    }
    Ok(IslandPlan {
        index,
        members: plans,
    })
}

/// For each island, the other islands that reach it over non-feedback
/// connections. Pairs of islands that reach each other are left out of
/// each other's lists, so neither waits on the other.
fn island_ancestors(
    compiled: &CompiledGraph,
    node_island: &HashMap<NodeId, usize>,
) -> Vec<Vec<usize>> {
    let mut preds: HashMap<&NodeId, Vec<&NodeId>> = HashMap::new();
    for conn in &compiled.graph().connections {
        if !conn.feedback {
            preds
                .entry(&conn.to.node)
                .or_default()
                .push(&conn.from.node);
        }
    }
    // The non-feedback graph is acyclic; topological order visits every
    // predecessor first.
    let mut node_ancestors: HashMap<&NodeId, HashSet<&NodeId>> = HashMap::new();
    for node in compiled.topological_order() {
        let mut set = HashSet::new();
        for pred in preds.get(node).into_iter().flatten() {
            set.insert(*pred);
            if let Some(more) = node_ancestors.get(pred) {
                set.extend(more.iter().copied());
            }
        }
        node_ancestors.insert(node, set);
    }

    let count = compiled.islands().len();
    let mut island_sets = vec![BTreeSet::new(); count];
    for (node, ancestors) in &node_ancestors {
        let Some(&island) = node_island.get(*node) else {
            continue;
        };
        for ancestor in ancestors {
            if let Some(&a) = node_island.get(*ancestor)
                && a != island
            {
                island_sets[island].insert(a);
            }
        }
    }
    (0..count)
        .map(|i| {
            island_sets[i]
                .iter()
                .copied()
                .filter(|&a| !island_sets[a].contains(&i))
                .collect()
        })
        .collect()
}
