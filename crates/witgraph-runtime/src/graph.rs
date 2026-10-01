//! The `RuntimeGraph` typestate: a compiled graph loaded with WASM
//! components, ready for execution.
//!
//! Extends the typestate chain: `GraphBuilder -> Graph -> CompiledGraph
//! -> RuntimeGraph`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use witgraph_ir::{CompiledGraph, ComponentRef, ConnectionId, NodeId, PortName, PortRef};

use crate::abi::{Activation, ActivationResult, InputSnapshot, OutputCollector, OutputWrite};
use crate::channel::{Channel, StreamPull};
use crate::engine::{
    create_linker, ActivationKind, NodeHostState, NodeInstance, WasmEngine, WitActivationResult,
};
use crate::error::{ChannelError, NodeFault, RuntimeError};
use crate::mode::{Release, RuntimeMode};
use crate::node::{Node, NodeState, SuspendedOutput};
use crate::resource::ResourcePool;
use crate::schedule::{SchedulerEvent, TickResult};
use witgraph_ir::Val;

/// A snapshot of all live Value-port input state, keyed by node id
/// then port name. Produced by [`RuntimeGraph::snapshot_values`],
/// consumed by [`Graph::apply_snapshot`](witgraph_ir::Graph::apply_snapshot).
pub type ValuesSnapshot = BTreeMap<NodeId, BTreeMap<PortName, Val>>;

/// Configuration for a runtime graph.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Default bounded channel capacity.
    pub channel_capacity: usize,
    /// Maximum scheduler steps per tick (safety limit).
    pub max_steps_per_tick: usize,
    /// Wasmtime fuel budget per node activation, if fuel metering is
    /// enabled.
    pub fuel_per_activation: Option<u64>,
    /// Epoch ticks before wasmtime epoch interruption, if epoch-based
    /// interruption is enabled.
    pub epoch_deadline: Option<u64>,
    /// Enable parallel activation of independent actors.
    pub concurrent: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 256,
            max_steps_per_tick: 10_000,
            fuel_per_activation: None,
            epoch_deadline: None,
            concurrent: false,
        }
    }
}

/// A write to a feedback edge, buffered during the actor loop and
/// committed at quiescence.
#[derive(Debug, Clone)]
pub(crate) enum FeedbackWrite {
    /// A latched value write.
    Value(Val),
    /// A discrete event payload.
    Event(Val),
    /// A stream item.
    StreamPush(Val),
    /// A stream close signal.
    StreamClose,
    /// A future resolution.
    FutureResolve(Val),
}

/// The outcome of committing a node's output writes to downstream
/// channels.
enum CommitOutcome {
    /// All writes committed successfully.
    Committed,
    /// A downstream bounded channel was full. The uncommitted writes
    /// are handed back so the caller can suspend the producing node
    /// with them.
    Backpressured(SuspendedOutput),
}

/// A compiled graph loaded with WASM component instances, ready for
/// event-driven execution.
///
/// Sealed: constructed only from [`CompiledGraph`] via
/// [`RuntimeGraph::load`]. The generic parameter `M` selects the
/// runtime mode ([`Release`] for zero-cost,
/// [`Debug`](crate::mode::Debug) for tracing).
pub struct RuntimeGraph<M: RuntimeMode = Release> {
    /// The underlying compiled graph.
    pub(crate) compiled: CompiledGraph,
    /// The WASM execution engine.
    pub(crate) engine: WasmEngine,
    /// Per-node WASM component instances.
    pub(crate) instances: HashMap<NodeId, NodeInstance>,
    /// Per-node lifecycle state.
    pub(crate) nodes: HashMap<NodeId, NodeState>,
    /// Per-connection channel.
    pub(crate) channels: HashMap<ConnectionId, Channel>,
    /// Input port to channel lookup.
    pub(crate) input_map: HashMap<PortRef, ConnectionId>,
    /// Output port to channel(s) lookup (fan-out).
    pub(crate) output_map: HashMap<PortRef, Vec<ConnectionId>>,
    /// Connection ids of feedback edges.
    pub(crate) feedback_edges: HashSet<ConnectionId>,
    /// Node to topological scheduling priority (lower = higher priority).
    pub(crate) topo_index: HashMap<NodeId, usize>,
    /// Priority-ready-queue of activatable nodes, keyed by
    /// (topo_index, NodeId).
    pub(crate) ready: BTreeSet<(usize, NodeId)>,
    /// Buffered feedback edge outputs, latched at quiescence.
    pub(crate) feedback_pending: Vec<(ConnectionId, FeedbackWrite)>,
    /// Connection ids whose one-shot activation (FutureResolved or
    /// StreamClosed) has been consumed, preventing duplicate activations.
    pub(crate) consumed_oneshots: HashSet<ConnectionId>,
    /// Connection id to target input port lookup.
    pub(crate) conn_target: HashMap<ConnectionId, PortRef>,
    /// Connection id to source output port lookup.
    pub(crate) conn_source: HashMap<ConnectionId, PortRef>,
    /// Per-node input connections, sorted by port name for deterministic
    /// iteration in `derive_activation`.
    pub(crate) node_inputs: HashMap<NodeId, Arc<[(PortRef, ConnectionId)]>>,
    /// Reverse lookup from a blocked connection to the suspended
    /// producer node waiting for capacity on that channel. The saved
    /// output itself lives in the node's [`Suspended`](crate::node::Suspended)
    /// state.
    pub(crate) suspended_on_conn: HashMap<ConnectionId, NodeId>,
    /// Pending scheduler events.
    pub(crate) pending: VecDeque<SchedulerEvent>,
    /// Nodes marked for cancellation, processed at the start of the
    /// next tick.
    pub(crate) cancelled: HashSet<NodeId>,
    /// Cancelled nodes that received new input and need
    /// re-instantiation, processed at the start of the next tick.
    pub(crate) restart_pending: HashSet<NodeId>,
    /// The configured component linker, retained for re-instantiation
    /// on restart.
    pub(crate) linker: wasmtime::component::Linker<NodeHostState>,
    /// Runtime config overrides for unconnected ports, set by
    /// [`inject`](Self::inject) when no channel exists.
    pub(crate) config_overrides: HashMap<PortRef, Val>,
    /// Pre-built index of compile-time config defaults per node.
    pub(crate) config_defaults: HashMap<NodeId, BTreeMap<PortName, Val>>,
    /// Runtime configuration.
    pub(crate) config: RuntimeConfig,
    /// The runtime mode instance.
    pub(crate) mode: M,
    /// Per-resource weighted semaphore for concurrency control.
    pub(crate) resources: ResourcePool,
    /// Nodes deferred because their resource claims could not be satisfied.
    pub(crate) resource_deferred: BTreeSet<(usize, NodeId)>,
}

impl<M: RuntimeMode> RuntimeGraph<M> {
    /// Loads a compiled graph with WASM component bytes, producing a
    /// runtime graph ready for execution.
    ///
    /// `wasm_modules` maps each component reference to its `.wasm`
    /// bytes. Every component instantiated by a node must have a
    /// corresponding entry.
    ///
    /// Validates the module map, creates the engine, compiles
    /// components, instantiates per-node WASM instances, allocates
    /// channels, and initialises node states.
    pub async fn load(
        compiled: CompiledGraph,
        wasm_modules: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
    ) -> Result<Self, RuntimeError> {
        Self::load_with_linker(compiled, wasm_modules, config, mode, |_| Ok(())).await
    }

    /// Like [`load`](Self::load), but accepts a callback to extend the
    /// linker with custom host interfaces before component instantiation.
    ///
    /// The callback receives the linker after the base `runtime-host`
    /// bindings have been registered. Use it to add implementations for
    /// any additional WIT imports your components require (e.g. an MQTT
    /// feed, a database query API, or other platform capabilities).
    pub async fn load_with_linker<F>(
        compiled: CompiledGraph,
        wasm_modules: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
        configure_linker: F,
    ) -> Result<Self, RuntimeError>
    where
        F: FnOnce(
            &mut wasmtime::component::Linker<NodeHostState>,
        ) -> Result<(), wasmtime::Error>,
    {
        if config.channel_capacity == 0 {
            return Err(RuntimeError::InvalidConfig {
                message: "channel_capacity must be at least 1".into(),
            });
        }

        // Validate that every component has a WASM module.
        for node in &compiled.graph().nodes {
            if let Some(contract) = compiled.contract_for(&node.id)
                && !wasm_modules.contains_key(&contract.id)
            {
                return Err(RuntimeError::MissingWasm {
                    component: Box::new(contract.id.clone()),
                });
            }
        }

        let mut engine = WasmEngine::new(
            config.fuel_per_activation.is_some(),
            config.epoch_deadline.is_some(),
        )
        .map_err(|e| RuntimeError::Instantiation {
            node: compiled
                .graph()
                .nodes
                .first()
                .map_or_else(|| NodeId::from("unknown"), |n| n.id.clone()),
            message: e.to_string(),
        })?;

        // Compile each unique component.
        for (comp_ref, bytes) in wasm_modules {
            engine
                .compile(comp_ref, bytes)
                .map_err(|e| RuntimeError::Instantiation {
                    node: compiled
                        .graph()
                        .nodes
                        .iter()
                        .find(|n| {
                            compiled
                                .contract_for(&n.id)
                                .is_some_and(|c| c.id == *comp_ref)
                        })
                        .map_or_else(|| NodeId::from("unknown"), |n| n.id.clone()),
                    message: e.to_string(),
                })?;
        }

        let mut linker =
            create_linker(engine.inner()).map_err(|e| RuntimeError::Instantiation {
                node: compiled
                    .graph()
                    .nodes
                    .first()
                    .map_or_else(|| NodeId::from("unknown"), |n| n.id.clone()),
                message: e.to_string(),
            })?;
        configure_linker(&mut linker).map_err(|e| RuntimeError::Instantiation {
            node: compiled
                .graph()
                .nodes
                .first()
                .map_or_else(|| NodeId::from("unknown"), |n| n.id.clone()),
            message: e.to_string(),
        })?;

        // Instantiate each node's component.
        let mut instances = HashMap::new();
        for node in &compiled.graph().nodes {
            if let Some(contract) = compiled.contract_for(&node.id) {
                let component = engine
                    .get_compiled(&contract.id)
                    .ok_or_else(|| RuntimeError::MissingWasm {
                        component: Box::new(contract.id.clone()),
                    })?
                    .clone();

                let instance = NodeInstance::instantiate(engine.inner(), &component, &linker)
                    .await
                    .map_err(|e| RuntimeError::Instantiation {
                        node: node.id.clone(),
                        message: e.to_string(),
                    })?;

                instances.insert(node.id.clone(), instance);
            }
        }

        // Build channel map, input/output maps, and feedback set.
        let mut channels = HashMap::new();
        let mut input_map = HashMap::new();
        let mut output_map: HashMap<PortRef, Vec<ConnectionId>> = HashMap::new();
        let mut feedback_edges = HashSet::new();

        for conn in &compiled.graph().connections {
            let port_kind = compiled
                .contract_for(&conn.from.node)
                .and_then(|contract| {
                    contract
                        .outputs
                        .iter()
                        .find(|p| p.name == conn.from.port)
                        .map(|p| p.kind)
                });

            if let Some(kind) = port_kind {
                channels.insert(
                    conn.id.clone(),
                    Channel::for_kind(kind, config.channel_capacity),
                );
            }

            input_map.insert(conn.to.clone(), conn.id.clone());
            output_map
                .entry(conn.from.clone())
                .or_default()
                .push(conn.id.clone());

            if conn.feedback {
                feedback_edges.insert(conn.id.clone());
            }
        }

        // Build reverse lookups: connection id -> target/source port.
        let conn_target: HashMap<ConnectionId, PortRef> = compiled
            .graph()
            .connections
            .iter()
            .map(|conn| (conn.id.clone(), conn.to.clone()))
            .collect();
        let conn_source: HashMap<ConnectionId, PortRef> = compiled
            .graph()
            .connections
            .iter()
            .map(|conn| (conn.id.clone(), conn.from.clone()))
            .collect();

        // Build per-node sorted input connection list.
        let mut node_inputs_build: HashMap<NodeId, Vec<(PortRef, ConnectionId)>> =
            HashMap::new();
        for (port_ref, conn_id) in &input_map {
            node_inputs_build
                .entry(port_ref.node.clone())
                .or_default()
                .push((port_ref.clone(), conn_id.clone()));
        }
        for entries in node_inputs_build.values_mut() {
            entries.sort_by(|a, b| a.0.port.cmp(&b.0.port));
        }
        let node_inputs: HashMap<NodeId, Arc<[(PortRef, ConnectionId)]>> = node_inputs_build
            .into_iter()
            .map(|(k, v)| (k, Arc::from(v)))
            .collect();

        // Build topological-index map from compilation order.
        let topo_index: HashMap<NodeId, usize> = compiled
            .topological_order()
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();

        // Build per-node state.
        let mut nodes = HashMap::new();
        for node in &compiled.graph().nodes {
            if let Some(contract) = compiled.contract_for(&node.id) {
                let mode_val = contract.consumption_mode();
                let drain_count = contract.inputs.iter().filter(|p| p.drained).count();
                nodes.insert(
                    node.id.clone(),
                    Node::new(node.id.clone(), mode_val, drain_count).into(),
                );
            }
        }

        // Seed the ready queue with source nodes: nodes whose every
        // connected input is either optional-and-unconnected or fed
        // exclusively by feedback edges.
        let mut ready = BTreeSet::new();
        for node in &compiled.graph().nodes {
            let Some(contract) = compiled.contract_for(&node.id) else {
                continue;
            };
            let is_source = contract.inputs.iter().all(|port_def| {
                let port_ref = PortRef::new(node.id.clone(), port_def.name.clone());
                match input_map.get(&port_ref) {
                    None => port_def.optional,
                    Some(conn_id) => feedback_edges.contains(conn_id),
                }
            });
            if is_source
                && let Some(&idx) = topo_index.get(&node.id)
            {
                ready.insert((idx, node.id.clone()));
            }
        }

        // Build config defaults index for O(1) lookup in build_input_snapshot.
        let config_defaults: HashMap<NodeId, BTreeMap<PortName, Val>> = compiled
            .graph()
            .nodes
            .iter()
            .filter(|n| !n.config.is_empty())
            .map(|n| (n.id.clone(), n.config.clone()))
            .collect();

        // Build resource pool from node declarations.
        let resource_entries: Vec<_> = compiled
            .graph()
            .nodes
            .iter()
            .filter(|n| !n.resources.is_empty())
            .map(|n| (n.id.clone(), n.resources.clone()))
            .collect();
        let resources = ResourcePool::from_nodes(&resource_entries);

        // Seed per-node config values into their Value channels.
        for node in &compiled.graph().nodes {
            for (port_name, val) in &node.config {
                let port_ref = PortRef::new(node.id.clone(), port_name.clone());
                if let Some(conn_id) = input_map.get(&port_ref)
                    && let Some(Channel::Value(slot)) = channels.get_mut(conn_id)
                {
                    slot.write(val.clone());
                }
            }
        }

        Ok(Self {
            compiled,
            engine,
            instances,
            nodes,
            channels,
            input_map,
            output_map,
            feedback_edges,
            topo_index,
            ready,
            feedback_pending: Vec::new(),
            consumed_oneshots: HashSet::new(),
            conn_target,
            conn_source,
            node_inputs,
            suspended_on_conn: HashMap::new(),
            pending: VecDeque::new(),
            cancelled: HashSet::new(),
            restart_pending: HashSet::new(),
            linker,
            config_overrides: HashMap::new(),
            config_defaults,
            config,
            mode,
            resources,
            resource_deferred: BTreeSet::new(),
        })
    }

    /// Injects an external input value into a node's port.
    pub fn inject(
        &mut self,
        node: NodeId,
        port: PortName,
        val: Val,
    ) -> Result<(), RuntimeError> {
        if !self.nodes.contains_key(&node) {
            return Err(RuntimeError::UnknownNode { node });
        }
        self.pending.push_back(SchedulerEvent::ExternalInput {
            node,
            port,
            value: val,
        });
        Ok(())
    }

    /// Reads a latched value output from a node's port.
    ///
    /// Returns `None` if the node, port, or connection does not exist,
    /// or if the value slot has not been written.
    pub fn read_output(&self, node: &NodeId, port: &PortName) -> Option<&Val> {
        let port_ref = PortRef::new(node.clone(), port.clone());
        let conn_ids = self.output_map.get(&port_ref)?;
        // For reading an output, we look at the first channel (the
        // output port itself may fan out; the value is the same).
        let conn_id = conn_ids.first()?;
        let channel = self.channels.get(conn_id)?;
        match channel {
            Channel::Value(slot) => slot.read(),
            _ => None,
        }
    }

    /// Inspects a node's current runtime state.
    pub fn node_state(&self, node: &NodeId) -> Option<&NodeState> {
        self.nodes.get(node)
    }

    /// Captures the current value of every Value input port across all
    /// nodes.
    ///
    /// For connected ports, reads the live channel value. For
    /// unconnected optional ports, falls back to the node's compile-time
    /// config. Ports with no value in either source are omitted.
    ///
    /// The returned snapshot can be applied to a [`Graph`](witgraph_ir::Graph)
    /// via [`Graph::apply_snapshot`](witgraph_ir::Graph::apply_snapshot)
    /// to restore these values as initial config on a fresh load.
    pub fn snapshot_values(&self) -> ValuesSnapshot {
        let mut snapshot = BTreeMap::new();
        for node in &self.compiled.graph().nodes {
            let Some(contract) = self.compiled.contract_for(&node.id) else {
                continue;
            };
            let mut ports = BTreeMap::new();
            for input in &contract.inputs {
                if input.kind != witgraph_ir::PortKind::Value {
                    continue;
                }
                let port_ref = PortRef::new(node.id.clone(), input.name.clone());
                let val = self
                    .input_map
                    .get(&port_ref)
                    .and_then(|conn_id| self.channels.get(conn_id))
                    .and_then(|ch| match ch {
                        Channel::Value(slot) => slot.read().cloned(),
                        _ => None,
                    })
                    .or_else(|| self.config_overrides.get(&port_ref).cloned())
                    .or_else(|| node.config.get(&input.name).cloned());
                if let Some(val) = val {
                    ports.insert(input.name.clone(), val);
                }
            }
            if !ports.is_empty() {
                snapshot.insert(node.id.clone(), ports);
            }
        }
        snapshot
    }

    /// Runs one round of the scheduler.
    ///
    /// Routes pending events to channels, then executes the actor loop:
    /// pops ready nodes in topological order, activates each one, and
    /// commits outputs to downstream channels. Nodes whose output
    /// channels are full are suspended rather than faulted; they resume
    /// automatically when the downstream consumer frees capacity.
    ///
    /// When the ready queue empties, latches any buffered feedback
    /// writes and returns [`TickResult::Progress`] so the caller can
    /// drive the next iteration.
    ///
    /// Async because wasmtime's Component Model 0.3 features require
    /// being driven by an async executor. The crate is
    /// executor-agnostic; the caller provides the executor.
    pub async fn tick(&mut self) -> TickResult {
        // Process pending cancellations.
        if let Some(result) = self.process_cancellations().await {
            return result;
        }

        // Re-instantiate cancelled nodes that received new input.
        if let Err(e) = self.process_restarts().await {
            return TickResult::Aborted {
                node: NodeId::from("unknown"),
                fault: NodeFault::WasmTrap {
                    message: e.to_string(),
                },
            };
        }

        // Step 1: Route pending events to channels and mark nodes
        // activatable.
        self.route_pending_events();

        // Step 2: Actor loop -- activate ready nodes in topological
        // order.
        let mut steps: usize = 0;

        while let Some((idx, node_id)) = self.ready.pop_first() {
            if steps >= self.config.max_steps_per_tick {
                // Re-insert the node that could not be processed.
                self.ready.insert((idx, node_id));
                return TickResult::StepLimitReached;
            }

            match self.nodes.get(&node_id) {
                None => continue,
                // Skip terminal nodes.
                Some(state) if state.is_terminal() => continue,
                // Resume a backpressure-suspended commit: retry the saved
                // output writes without re-executing WASM activation.
                Some(NodeState::Suspended(_)) => {
                    if let Some(aborted) = self.resume_suspended(&node_id).await {
                        return aborted;
                    }
                    steps += 1;
                    continue;
                }
                Some(_) => {}
            }

            // Resource budget gate: defer if claims cannot be satisfied.
            if !self.resources.is_held(&node_id) {
                if !self.resources.can_acquire(&node_id) {
                    self.resource_deferred.insert((idx, node_id));
                    continue;
                }
                self.resources.acquire(&node_id);
            }

            // Call init() on first activation. A node that has been
            // initialized is `Draining` or `Ready`.
            if matches!(self.nodes.get(&node_id), Some(NodeState::Created(_))) {
                if let Err(fault) = self.call_node_init(&node_id).await {
                    if let Some(aborted) = self.fail_node(&node_id, fault, false).await {
                        return aborted;
                    }
                    continue;
                }
                self.transition(&node_id, |s| s.map_created(Node::initialized));
            }

            self.transition(&node_id, NodeState::activate);

            match self.activate_and_commit(&node_id).await {
                Ok((CommitOutcome::Committed, activation_result)) => {
                    self.apply_post_commit(&node_id, activation_result).await;
                }
                Ok((CommitOutcome::Backpressured(out), _)) => {
                    self.resources.release_non_held(&node_id);
                    ResourcePool::drain_deferred_into(
                        &mut self.resource_deferred,
                        &mut self.ready,
                    );
                    self.suspend(&node_id, out);
                }
                Err(fault) => {
                    if let Some(aborted) = self.fail_node(&node_id, fault, true).await {
                        return aborted;
                    }
                }
            }

            steps += 1;
        }

        // Merge resource-deferred nodes back into ready for the next tick.
        let had_deferred = !self.resource_deferred.is_empty();
        self.ready.append(&mut self.resource_deferred);

        // Step 3: Feedback latch at quiescence.
        if !self.feedback_pending.is_empty() {
            self.latch_feedback();
            return TickResult::Progress;
        }

        // Pending restarts or cancellations mean more work on the
        // next tick — signal progress so the caller loops.
        if !self.restart_pending.is_empty() || !self.cancelled.is_empty() {
            return TickResult::Progress;
        }

        if self.nodes.values().all(NodeState::is_terminal) {
            return TickResult::Completed;
        }

        if steps > 0 || had_deferred {
            TickResult::Progress
        } else {
            TickResult::Idle
        }
    }

    /// Returns a reference to the underlying compiled graph.
    pub fn compiled(&self) -> &CompiledGraph {
        &self.compiled
    }

    /// Returns a reference to the runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Returns a reference to the runtime mode.
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// Returns a reference to the WASM engine.
    pub fn engine(&self) -> &WasmEngine {
        &self.engine
    }

    /// Returns the input-port-to-channel-id lookup.
    pub fn input_map(&self) -> &HashMap<PortRef, ConnectionId> {
        &self.input_map
    }

    /// Returns the set of feedback edge connection ids.
    pub fn feedback_edges(&self) -> &HashSet<ConnectionId> {
        &self.feedback_edges
    }

    /// Marks a node for cancellation. The node is disposed and
    /// transitioned to [`Cancelled`](crate::node::Cancelled) at the start of the
    /// next [`tick`](Self::tick). Cancellation propagates upstream:
    /// producers whose every downstream consumer is terminal are
    /// cancelled automatically.
    pub fn cancel(&mut self, node_id: &NodeId) -> Result<(), RuntimeError> {
        let state = self.nodes.get(node_id).ok_or(RuntimeError::UnknownNode {
            node: node_id.clone(),
        })?;
        if !state.is_terminal() {
            self.cancelled.insert(node_id.clone());
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Private scheduler helpers
    // ------------------------------------------------------------------

    /// Routes all pending [`SchedulerEvent`]s to channels and inserts
    /// affected nodes into the ready queue.
    fn route_pending_events(&mut self) {
        while let Some(event) = self.pending.pop_front() {
            match event {
                SchedulerEvent::ExternalInput { node, port, value } => {
                    let port_ref = PortRef::new(node.clone(), port.clone());
                    if let Some(conn_id) = self.input_map.get(&port_ref)
                        && let Some(channel) = self.channels.get_mut(conn_id)
                    {
                        let write_err = match channel {
                            Channel::Value(slot) => {
                                slot.write(value);
                                None
                            }
                            Channel::Event(queue) => queue.enqueue(value).err(),
                            Channel::Stream(stream) => stream.push(value).err(),
                            Channel::Future(slot) => slot.resolve(value).err(),
                        };
                        if let Some(err) = write_err {
                            let fault = channel_error_to_fault(&port, err);
                            self.fault_node(&node, &fault);
                            self.propagate_cancellation(&node);
                            continue;
                        }
                    } else {
                        self.config_overrides.insert(port_ref, value);
                    }
                    self.enqueue_node(&node);
                }
                SchedulerEvent::ValueChanged { target }
                | SchedulerEvent::EventEnqueued { target }
                | SchedulerEvent::StreamItem { target }
                | SchedulerEvent::StreamClosed { target }
                | SchedulerEvent::FutureResolved { target } => {
                    self.enqueue_node(&target.node);
                }
            }
        }
    }

    /// Calls the node's WASM `init()` export.
    ///
    /// Called once when a node transitions from `Created` to `Ready`.
    async fn call_node_init(&mut self, node_id: &NodeId) -> Result<(), NodeFault> {
        let instance = match self.instances.get_mut(node_id) {
            Some(inst) => inst,
            None => return Ok(()),
        };
        instance.call_init().await.map_err(wasmtime_error_to_fault)
    }

    /// Calls the node's WASM `dispose()` export.
    ///
    /// Called when a node transitions to `Completed` or `Faulted`.
    async fn call_node_dispose(&mut self, node_id: &NodeId) -> Result<(), NodeFault> {
        let instance = match self.instances.get_mut(node_id) {
            Some(inst) => inst,
            None => return Ok(()),
        };
        instance
            .call_dispose()
            .await
            .map_err(wasmtime_error_to_fault)
    }

    /// Activates a node, returning the activation result and the
    /// collected output writes.
    ///
    /// Builds an [`InputSnapshot`], fires mode callbacks, converts the
    /// [`Activation`] to a WIT [`ActivationKind`], and calls the
    /// node's WASM `activate()` export. Converts the WIT result back
    /// to an [`ActivationResult`].
    async fn activate_node(
        &mut self,
        node_id: &NodeId,
    ) -> Result<(ActivationResult, OutputCollector), NodeFault> {
        let snapshot = self.build_input_snapshot(node_id);
        let activation = self.derive_activation(node_id);
        let wit_kind = activation_to_wit(&activation)?;

        self.mode.on_before_activate(node_id, &activation);

        let instance = match self.instances.get_mut(node_id) {
            Some(inst) => inst,
            None => {
                // No instance bound: return Continue with empty
                // collector (graceful degradation).
                let result = ActivationResult::Continue;
                self.mode.on_after_activate(node_id, &result);
                return Ok((result, OutputCollector::new()));
            }
        };

        let _ = instance.prepare_activation(
            snapshot,
            self.config.fuel_per_activation,
            self.config.epoch_deadline,
            false,
        );

        // Call the WASM activate() export.
        let wit_result = instance
            .call_activate(&wit_kind)
            .await
            .map_err(wasmtime_error_to_fault)?;

        // Extract the collector from the store.
        let collector = instance.take_collector();

        let result = wit_activation_result_to_ours(wit_result);
        self.mode.on_after_activate(node_id, &result);

        Ok((result, collector))
    }

    /// Activates a node and commits the outputs it produced.
    ///
    /// A `fatal()` signal from the node is reported as
    /// [`NodeFault::Fatal`], like any other fault.
    async fn activate_and_commit(
        &mut self,
        node_id: &NodeId,
    ) -> Result<(CommitOutcome, ActivationResult), NodeFault> {
        let (activation_result, collector) = self.activate_node(node_id).await?;
        if let Some(message) = collector.fatal() {
            return Err(NodeFault::Fatal {
                message: message.to_string(),
            });
        }
        let outcome = self.commit_outputs(node_id, collector, activation_result)?;
        Ok((outcome, activation_result))
    }

    /// Commits an [`OutputCollector`] produced by a node activation.
    ///
    /// Delegates to [`commit_writes`](Self::commit_writes). When a
    /// downstream bounded channel is full, the uncommitted writes are
    /// saved and [`CommitOutcome::Backpressured`] is returned so the
    /// caller can suspend the node.
    fn commit_outputs(
        &mut self,
        node_id: &NodeId,
        collector: OutputCollector,
        activation_result: ActivationResult,
    ) -> Result<CommitOutcome, NodeFault> {
        self.commit_writes(node_id, collector.drain(), None, activation_result)
    }

    /// Commits a sequence of output writes to downstream channels.
    ///
    /// For each write, the method looks up the output port's downstream
    /// connections via [`output_map`](Self::output_map). Feedback edges
    /// are buffered in [`feedback_pending`](Self::feedback_pending).
    /// Non-feedback edges are written to their target channels and the
    /// downstream node is inserted into the ready queue.
    ///
    /// When a bounded channel (event queue or stream) is full, the
    /// current write and all remaining writes are returned in
    /// [`CommitOutcome::Backpressured`] for the caller to suspend the
    /// node with. Non-capacity errors (double-resolve,
    /// write-after-close) remain hard faults.
    ///
    /// `first_pending_conns` provides the fan-out connections for the
    /// first write when resuming a partially committed output. For
    /// fresh commits, pass `None` to use the full fan-out set.
    fn commit_writes(
        &mut self,
        node_id: &NodeId,
        writes: Vec<OutputWrite>,
        mut first_pending_conns: Option<Vec<ConnectionId>>,
        activation_result: ActivationResult,
    ) -> Result<CommitOutcome, NodeFault> {
        let mut remaining: VecDeque<OutputWrite> = VecDeque::from(writes);
        let owned_node_id = node_id.clone();
        let mut last_source_ref: Option<PortRef> = None;

        while let Some(output_write) = remaining.pop_front() {
            let port = output_write_port(&output_write).clone();
            let source_ref = match &last_source_ref {
                Some(r) if r.port == port => r.clone(),
                _ => {
                    let r = PortRef::new(owned_node_id.clone(), port.clone());
                    last_source_ref = Some(r.clone());
                    r
                }
            };

            // Use the provided fan-out connections for the first write
            // (when resuming a partial commit), otherwise look up the
            // full fan-out set from output_map. `take()` ensures
            // subsequent writes always use output_map.
            let conn_ids = if let Some(conns) = first_pending_conns.take() {
                conns
            } else {
                match self.output_map.get(&source_ref) {
                    Some(ids) => ids.clone(),
                    None => continue,
                }
            };

            for (conn_offset, conn_id) in conn_ids.iter().enumerate() {
                // Feedback edges: buffer instead of committing.
                if self.feedback_edges.contains(conn_id) {
                    let fb = match &output_write {
                        OutputWrite::Value { value, .. } => {
                            FeedbackWrite::Value(value.clone())
                        }
                        OutputWrite::Event { payload, .. } => {
                            FeedbackWrite::Event(payload.clone())
                        }
                        OutputWrite::StreamPush { item, .. } => {
                            FeedbackWrite::StreamPush(item.clone())
                        }
                        OutputWrite::StreamClose { .. } => FeedbackWrite::StreamClose,
                        OutputWrite::FutureResolve { value, .. } => {
                            FeedbackWrite::FutureResolve(value.clone())
                        }
                    };
                    self.feedback_pending.push((conn_id.clone(), fb));
                    continue;
                }

                let target = match self.conn_target.get(conn_id) {
                    Some(t) => t.clone(),
                    None => continue,
                };

                let channel = match self.channels.get_mut(conn_id) {
                    Some(ch) => ch,
                    None => continue,
                };

                // Attempt the channel write. Returns `true` if the
                // bounded channel was at capacity (backpressure).
                let blocked = match (&output_write, channel) {
                    (OutputWrite::Value { value, .. }, Channel::Value(slot)) => {
                        slot.write(value.clone());
                        self.mode.on_channel_write(&source_ref, &target, value);
                        false
                    }
                    (OutputWrite::Event { payload, .. }, Channel::Event(queue)) => {
                        match queue.enqueue(payload.clone()) {
                            Ok(()) => {
                                self.mode.on_channel_write(
                                    &source_ref,
                                    &target,
                                    payload,
                                );
                                false
                            }
                            Err(ChannelError::Full(_)) => true,
                            Err(e) => return Err(channel_error_to_fault(&port, e)),
                        }
                    }
                    (OutputWrite::StreamPush { item, .. }, Channel::Stream(stream)) => {
                        match stream.push(item.clone()) {
                            Ok(()) => {
                                self.mode
                                    .on_channel_write(&source_ref, &target, item);
                                false
                            }
                            Err(ChannelError::Full(_)) => true,
                            Err(e) => return Err(channel_error_to_fault(&port, e)),
                        }
                    }
                    (OutputWrite::StreamClose { .. }, Channel::Stream(stream)) => {
                        stream
                            .close()
                            .map_err(|e| channel_error_to_fault(&port, e))?;
                        false
                    }
                    (OutputWrite::FutureResolve { value, .. }, Channel::Future(slot)) => {
                        slot.resolve(value.clone())
                            .map_err(|e| channel_error_to_fault(&port, e))?;
                        self.mode.on_channel_write(&source_ref, &target, value);
                        false
                    }
                    _ => {
                        // Channel kind does not match the write kind.
                        // This indicates a graph construction error;
                        // skip silently.
                        continue;
                    }
                };

                if blocked {
                    // Save the current write (with the remaining
                    // fan-out connections) and all subsequent writes.
                    let pending_conns = conn_ids[conn_offset..].to_vec();
                    let mut saved_writes =
                        Vec::with_capacity(remaining.len() + 1);
                    saved_writes.push(output_write);
                    saved_writes.extend(remaining.drain(..));

                    return Ok(CommitOutcome::Backpressured(SuspendedOutput {
                        activation_result,
                        blocked_conn: conn_id.clone(),
                        pending_conns,
                        writes: saved_writes,
                    }));
                }

                // Enqueue the downstream node.
                self.enqueue_node(&target.node);
            }
        }

        Ok(CommitOutcome::Committed)
    }

    /// Applies the post-commit actions for a node whose outputs have
    /// been fully committed.
    ///
    /// For [`ActivationResult::Continue`], the node goes back to
    /// [`Draining`](crate::node::Draining) if it still has drains
    /// pending and to [`Ready`](crate::node::Ready) otherwise, and is
    /// re-enqueued if input data is pending. For
    /// [`ActivationResult::Completed`], the node completes and is
    /// disposed.
    async fn apply_post_commit(
        &mut self,
        node_id: &NodeId,
        activation_result: ActivationResult,
    ) {
        match activation_result {
            ActivationResult::Continue => {
                self.transition(node_id, |s| s.map_running(Node::proceed));
                self.resources.release(node_id);
                ResourcePool::drain_deferred_into(
                    &mut self.resource_deferred,
                    &mut self.ready,
                );
                if self.has_pending_input(node_id) {
                    self.enqueue_node(node_id);
                }
            }
            ActivationResult::Completed => {
                self.transition(node_id, |s| s.map_running(|n| n.complete().into()));
                self.resources.release(node_id);
                ResourcePool::drain_deferred_into(
                    &mut self.resource_deferred,
                    &mut self.ready,
                );
                let _ = self.call_node_dispose(node_id).await;
                self.propagate_cancellation(node_id);
            }
        }
    }

    /// Latches all buffered feedback writes to their target channels
    /// and enqueues affected downstream nodes.
    fn latch_feedback(&mut self) {
        let pending = std::mem::take(&mut self.feedback_pending);
        for (conn_id, fb_write) in pending {
            let target = match self.conn_target.get(&conn_id) {
                Some(t) => t.clone(),
                None => continue,
            };

            let channel = match self.channels.get_mut(&conn_id) {
                Some(ch) => ch,
                None => continue,
            };

            match (fb_write, channel) {
                (FeedbackWrite::Value(val), Channel::Value(slot)) => {
                    slot.write(val);
                }
                (FeedbackWrite::Event(val), Channel::Event(queue)) => {
                    if let Err(ChannelError::Full(cap)) = queue.enqueue(val) {
                        self.mode.on_node_fault(
                            &target.node,
                            &NodeFault::ChannelOverflow {
                                port: target.port.clone(),
                                capacity: cap,
                            },
                        );
                    }
                }
                (FeedbackWrite::StreamPush(val), Channel::Stream(stream)) => {
                    if let Err(ChannelError::Full(cap)) = stream.push(val) {
                        self.mode.on_node_fault(
                            &target.node,
                            &NodeFault::ChannelOverflow {
                                port: target.port.clone(),
                                capacity: cap,
                            },
                        );
                    }
                }
                (FeedbackWrite::StreamClose, Channel::Stream(stream)) => {
                    let _ = stream.close();
                }
                (FeedbackWrite::FutureResolve(val), Channel::Future(slot)) => {
                    if slot.resolve(val).is_err() {
                        self.mode.on_node_fault(
                            &target.node,
                            &NodeFault::DoubleResolve {
                                port: target.port.clone(),
                            },
                        );
                        continue;
                    }
                }
                _ => continue,
            }

            self.enqueue_node(&target.node);
        }
    }

    /// Builds a read-only [`InputSnapshot`] of a node's current latched
    /// value inputs.
    ///
    /// Reads connected Value channels first, then fills in any missing
    /// ports from the node's compile-time config (covers unconnected
    /// optional ports with initial values).
    fn build_input_snapshot(&self, node_id: &NodeId) -> InputSnapshot {
        let mut snapshot = InputSnapshot::new();
        if let Some(entries) = self.node_inputs.get(node_id) {
            for (port_ref, conn_id) in entries.iter() {
                if let Some(Channel::Value(slot)) = self.channels.get(conn_id)
                    && let Some(val) = slot.read()
                {
                    snapshot.insert(port_ref.port.clone(), val.clone());
                }
            }
        }
        // Layer 2: runtime config overrides (from inject on unconnected ports).
        for (port_ref, val) in &self.config_overrides {
            if port_ref.node == *node_id && !snapshot.contains(&port_ref.port) {
                snapshot.insert(port_ref.port.clone(), val.clone());
            }
        }
        // Layer 3: compile-time config defaults.
        if let Some(defaults) = self.config_defaults.get(node_id) {
            for (port_name, val) in defaults {
                if !snapshot.contains(port_name) {
                    snapshot.insert(port_name.clone(), val.clone());
                }
            }
        }
        snapshot
    }

    /// Derives the activation kind from the node's input channel state.
    ///
    /// Scans the node's input channels in deterministic (port-name-sorted)
    /// order. For a node that still has drains pending, checks drained
    /// input ports first: pulls one item from a drained stream or clones
    /// a resolved future, returning [`Activation::DrainItem`]. When a
    /// drained stream is closed and empty, or a drained future resolves,
    /// the running node's pending drain count is decremented; once it
    /// reaches zero the node goes to [`Ready`](crate::node::Ready) when
    /// the activation finishes (see [`Node::proceed`]).
    ///
    /// For non-draining (or drain-complete) nodes, checks for pending
    /// events, stream items, stream closes, and future resolutions in
    /// port-name order, returning the first match. Falls back to
    /// [`Activation::Sync`] when no async input data is pending.
    fn derive_activation(&mut self, node_id: &NodeId) -> Activation {
        let empty: Arc<[(PortRef, ConnectionId)]> = Arc::from([]);
        let entries = self
            .node_inputs
            .get(node_id)
            .cloned()
            .unwrap_or(empty);

        let is_draining = self
            .nodes
            .get(node_id)
            .is_some_and(|s| s.pending_drains() > 0);

        if is_draining {
            // Collect the set of drained port names from the contract
            // before mutably borrowing channels.
            let drained_ports: HashSet<PortName> = self
                .compiled
                .contract_for(node_id)
                .map(|c| {
                    c.inputs
                        .iter()
                        .filter(|p| p.drained)
                        .map(|p| p.name.clone())
                        .collect()
                })
                .unwrap_or_default();

            for (port_ref, conn_id) in entries.iter() {
                if self.feedback_edges.contains(conn_id) {
                    continue;
                }
                if !drained_ports.contains(&port_ref.port) {
                    continue;
                }
                if self.consumed_oneshots.contains(conn_id) {
                    continue;
                }

                match self.channels.get_mut(conn_id) {
                    Some(Channel::Stream(stream)) => {
                        if !stream.is_empty() {
                            if let StreamPull::Item(val) = stream.pull() {
                                // Re-enqueue any producer suspended on
                                // this connection (capacity freed).
                                if let Some(pid) =
                                    self.suspended_on_conn.get(conn_id)
                                    && let Some(&ti) =
                                        self.topo_index.get(pid)
                                {
                                    self.ready
                                        .insert((ti, pid.clone()));
                                }
                                return Activation::DrainItem {
                                    port: port_ref.port.clone(),
                                    item: val,
                                };
                            }
                        } else if stream.is_closed() {
                            self.complete_drain(node_id);
                            self.consumed_oneshots.insert(conn_id.clone());
                        }
                    }
                    Some(Channel::Future(slot)) => {
                        if let Some(val) = slot.poll() {
                            let val = val.clone();
                            self.complete_drain(node_id);
                            self.consumed_oneshots.insert(conn_id.clone());
                            return Activation::DrainItem {
                                port: port_ref.port.clone(),
                                item: val,
                            };
                        }
                    }
                    _ => {}
                }
            }

            // If drains remain there is nothing else to deliver yet. When
            // the last one just completed, fall through to deliver any
            // other pending input in this same activation.
            let drains_pending = self
                .nodes
                .get(node_id)
                .is_some_and(|s| s.pending_drains() > 0);
            if drains_pending {
                return Activation::Sync;
            }
        }

        // Non-draining or drain-complete: scan for async input data.
        for (port_ref, conn_id) in entries.iter() {
            if self.feedback_edges.contains(conn_id) {
                continue;
            }

            match self.channels.get_mut(conn_id) {
                Some(Channel::Event(queue)) => {
                    if let Some(val) = queue.dequeue() {
                        // Re-enqueue any producer suspended on this
                        // connection (capacity freed).
                        if let Some(pid) =
                            self.suspended_on_conn.get(conn_id)
                            && let Some(&ti) = self.topo_index.get(pid)
                        {
                            self.ready.insert((ti, pid.clone()));
                        }
                        return Activation::Event {
                            port: port_ref.port.clone(),
                            payload: val,
                        };
                    }
                }
                Some(Channel::Stream(stream)) => {
                    if !stream.is_empty() {
                        if let StreamPull::Item(val) = stream.pull() {
                            // Re-enqueue any producer suspended on
                            // this connection (capacity freed).
                            if let Some(pid) =
                                self.suspended_on_conn.get(conn_id)
                                && let Some(&ti) =
                                    self.topo_index.get(pid)
                            {
                                self.ready
                                    .insert((ti, pid.clone()));
                            }
                            return Activation::StreamItem {
                                port: port_ref.port.clone(),
                                item: val,
                            };
                        }
                    } else if stream.is_closed()
                        && !self.consumed_oneshots.contains(conn_id)
                    {
                        self.consumed_oneshots.insert(conn_id.clone());
                        return Activation::StreamClosed {
                            port: port_ref.port.clone(),
                        };
                    }
                }
                Some(Channel::Future(slot))
                    if slot.is_resolved()
                        && !self.consumed_oneshots.contains(conn_id) =>
                {
                    if let Some(val) = slot.poll() {
                        let val = val.clone();
                        self.consumed_oneshots.insert(conn_id.clone());
                        return Activation::FutureResolved {
                            port: port_ref.port.clone(),
                            value: val,
                        };
                    }
                }
                _ => {} // Value channels are read via InputSnapshot.
            }
        }

        Activation::Sync
    }

    /// Checks whether any non-Value, non-feedback input channel for the
    /// node has pending data that requires an activation.
    ///
    /// Returns `true` if any input event queue is non-empty, any stream
    /// channel has buffered items or an unconsumed close signal, or any
    /// future slot is resolved but not yet consumed.
    fn has_pending_input(&self, node_id: &NodeId) -> bool {
        let Some(entries) = self.node_inputs.get(node_id) else {
            return false;
        };
        for (_, conn_id) in entries.iter() {
            if self.feedback_edges.contains(conn_id) {
                continue;
            }
            match self.channels.get(conn_id) {
                Some(Channel::Event(queue)) => {
                    if !queue.is_empty() {
                        return true;
                    }
                }
                Some(Channel::Stream(stream)) => {
                    if !stream.is_empty() {
                        return true;
                    }
                    if stream.is_closed()
                        && !self.consumed_oneshots.contains(conn_id)
                    {
                        return true;
                    }
                }
                Some(Channel::Future(slot))
                    if slot.is_resolved()
                        && !self.consumed_oneshots.contains(conn_id) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Applies a lifecycle transition to a node, firing the mode
    /// callback if its phase changed.
    ///
    /// `f` receives the node's current state and returns its next one;
    /// the typed transitions on [`Node`] and the `map_*` methods on
    /// [`NodeState`] are what it is built from.
    fn transition(&mut self, node_id: &NodeId, f: impl FnOnce(NodeState) -> NodeState) {
        Self::transition_in(&mut self.nodes, &self.mode, node_id, f);
    }

    /// [`transition`](Self::transition) over the individual fields it
    /// touches, for callers that hold other borrows of `self`.
    fn transition_in(
        nodes: &mut HashMap<NodeId, NodeState>,
        mode: &M,
        node_id: &NodeId,
        f: impl FnOnce(NodeState) -> NodeState,
    ) {
        let Some((key, state)) = nodes.remove_entry(node_id) else {
            return;
        };
        let from = state.phase();
        let next = f(state);
        let to = next.phase();
        nodes.insert(key, next);
        if from != to {
            mode.on_phase_transition(node_id, from, to);
        }
    }

    /// Records that one of a running node's drained inputs completed.
    fn complete_drain(&mut self, node_id: &NodeId) {
        if let Some(NodeState::Running(node)) = self.nodes.get_mut(node_id) {
            node.complete_drain();
        }
    }

    /// Suspends a running node whose outputs hit downstream
    /// backpressure, and registers it to be woken when the blocking
    /// channel frees capacity.
    fn suspend(&mut self, node_id: &NodeId, out: SuspendedOutput) {
        self.suspended_on_conn
            .insert(out.blocked_conn.clone(), node_id.clone());
        self.transition(node_id, |s| s.map_running(|n| n.suspend(out).into()));
    }

    /// Retries committing the output of a backpressure-suspended node.
    ///
    /// The node stays suspended if a channel is still full. Returns
    /// `Some(TickResult::Aborted)` if the node faulted fatally.
    async fn resume_suspended(&mut self, node_id: &NodeId) -> Option<TickResult> {
        let mut saved = None;
        self.transition(node_id, |s| {
            s.map_suspended(|n| {
                let (running, out) = n.resume();
                saved = Some(out);
                running.into()
            })
        });
        let out = saved?;
        self.suspended_on_conn.remove(&out.blocked_conn);

        let activation_result = out.activation_result;
        match self.commit_writes(
            node_id,
            out.writes,
            Some(out.pending_conns),
            activation_result,
        ) {
            Ok(CommitOutcome::Committed) => {
                self.apply_post_commit(node_id, activation_result).await;
                None
            }
            // Still blocked on a (possibly different) channel.
            Ok(CommitOutcome::Backpressured(out)) => {
                self.suspend(node_id, out);
                None
            }
            Err(fault) => self.fail_node(node_id, fault, true).await,
        }
    }

    /// Records a node fault: releases the node's resources, notifies the
    /// mode, and moves the node to [`Faulted`](crate::node::Faulted).
    fn fault_node(&mut self, node_id: &NodeId, fault: &NodeFault) {
        self.resources.release(node_id);
        ResourcePool::drain_deferred_into(&mut self.resource_deferred, &mut self.ready);
        self.mode.on_node_fault(node_id, fault);
        self.transition(node_id, |s| s.fault(fault.clone()));
    }

    /// Handles a fault raised while running a node.
    ///
    /// Faults the node, disposes it if `dispose` is set, then either
    /// reports a fatal fault as `Some(TickResult::Aborted)` or
    /// propagates cancellation upstream and returns `None`.
    async fn fail_node(
        &mut self,
        node_id: &NodeId,
        fault: NodeFault,
        dispose: bool,
    ) -> Option<TickResult> {
        self.fault_node(node_id, &fault);
        if dispose {
            let _ = self.call_node_dispose(node_id).await;
        }
        if matches!(fault, NodeFault::Fatal { .. }) {
            return Some(TickResult::Aborted {
                node: node_id.clone(),
                fault,
            });
        }
        self.propagate_cancellation(node_id);
        None
    }

    /// Inserts a node into the ready queue using its topological index.
    /// If the node is cancelled, marks it for restart instead.
    fn enqueue_node(&mut self, node_id: &NodeId) {
        if let Some(NodeState::Cancelled(_)) = self.nodes.get(node_id) {
            self.restart_pending.insert(node_id.clone());
            return;
        }
        if let Some(&idx) = self.topo_index.get(node_id) {
            self.ready.insert((idx, node_id.clone()));
        }
    }

    /// Processes all pending cancellations: disposes each cancelled
    /// node, transitions it to [`Cancelled`](crate::node::Cancelled), and
    /// propagates cancellation upstream.
    ///
    /// Returns `Some(TickResult::Aborted)` if a fatal fault is
    /// encountered during dispose; otherwise `None`.
    async fn process_cancellations(&mut self) -> Option<TickResult> {
        loop {
            let batch: Vec<NodeId> = self.cancelled.drain().collect();
            if batch.is_empty() {
                return None;
            }
            for node_id in batch {
                if self
                    .nodes
                    .get(&node_id)
                    .is_some_and(NodeState::is_terminal)
                {
                    continue;
                }

                // A suspended node is no longer waiting on its blocked
                // channel (its saved output is dropped with the state).
                if let Some(NodeState::Suspended(node)) = self.nodes.get(&node_id) {
                    self.suspended_on_conn.remove(node.blocked_on());
                }

                // Release any held resources.
                self.resources.release(&node_id);
                ResourcePool::drain_deferred_into(
                    &mut self.resource_deferred,
                    &mut self.ready,
                );

                // Remove from the ready queue.
                if let Some(&idx) = self.topo_index.get(&node_id) {
                    self.ready.remove(&(idx, node_id.clone()));
                }

                self.mode.on_cancelled(&node_id);
                self.transition(&node_id, NodeState::cancel);

                if let Err(fault) = self.call_node_dispose(&node_id).await
                    && matches!(fault, NodeFault::Fatal { .. })
                {
                    return Some(TickResult::Aborted {
                        node: node_id,
                        fault,
                    });
                }

                self.propagate_cancellation(&node_id);
            }
        }
    }

    /// Checks whether every downstream consumer of a node is terminal.
    /// If so, marks the node for cancellation (upstream propagation).
    fn propagate_cancellation(&mut self, terminated_node: &NodeId) {
        let empty: Arc<[(PortRef, ConnectionId)]> = Arc::from([]);
        let inputs = self.node_inputs.get(terminated_node).unwrap_or(&empty);
        let upstream_producers: Vec<NodeId> = inputs
            .iter()
            .filter_map(|(_, conn_id)| {
                self.conn_source.get(conn_id).map(|src| src.node.clone())
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        for producer_id in upstream_producers {
            if self
                .nodes
                .get(&producer_id)
                .is_some_and(NodeState::is_terminal)
            {
                continue;
            }
            if self.all_consumers_terminal(&producer_id) {
                self.cancelled.insert(producer_id);
            }
        }
    }

    /// Returns `true` if every downstream consumer of the given node
    /// (across all its output ports) is in a terminal phase.
    fn all_consumers_terminal(&self, node_id: &NodeId) -> bool {
        let Some(contract) = self.compiled.contract_for(node_id) else {
            return false;
        };
        for output in &contract.outputs {
            let port_ref = PortRef::new(node_id.clone(), output.name.clone());
            let Some(conn_ids) = self.output_map.get(&port_ref) else {
                continue;
            };
            for conn_id in conn_ids {
                if let Some(target) = self.conn_target.get(conn_id) {
                    let is_terminal = self
                        .nodes
                        .get(&target.node)
                        .is_some_and(NodeState::is_terminal);
                    if !is_terminal {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Re-instantiates all nodes in `restart_pending`: creates a fresh
    /// WASM instance, restarts the node from [`Created`](crate::node::Created),
    /// resets output channels, and enqueues the node.
    async fn process_restarts(&mut self) -> Result<(), RuntimeError> {
        let batch: Vec<NodeId> = self.restart_pending.drain().collect();
        for (i, node_id) in batch.iter().enumerate() {
            // Only cancelled nodes are queued for restart, and nothing
            // moves a terminal node out of its phase in between.
            if !matches!(self.nodes.get(node_id), Some(NodeState::Cancelled(_))) {
                continue;
            }
            let Some(contract) = self.compiled.contract_for(node_id) else {
                continue;
            };
            let component = match self
                .engine
                .get_compiled(&contract.id)
            {
                Some(c) => c.clone(),
                None => {
                    self.restart_pending.extend(batch[i + 1..].iter().cloned());
                    return Err(RuntimeError::MissingWasm {
                        component: Box::new(contract.id.clone()),
                    });
                }
            };

            let instance = match
                NodeInstance::instantiate(self.engine.inner(), &component, &self.linker)
                    .await
            {
                Ok(inst) => inst,
                Err(e) => {
                    self.restart_pending.extend(batch[i + 1..].iter().cloned());
                    return Err(RuntimeError::Instantiation {
                        node: node_id.clone(),
                        message: e.to_string(),
                    });
                }
            };
            self.instances.insert(node_id.clone(), instance);

            // Restart the node from `Created`.
            let drain_count = contract.inputs.iter().filter(|p| p.drained).count();
            Self::transition_in(&mut self.nodes, &self.mode, node_id, |s| {
                s.map_cancelled(|n| n.restart(drain_count).into())
            });

            // Reset output channels only where the downstream consumer is
            // terminal. Channels with live downstream consumers keep their
            // committed data to prevent data loss.
            for output in &contract.outputs {
                let port_ref = PortRef::new(node_id.clone(), output.name.clone());
                if let Some(conn_ids) = self.output_map.get(&port_ref) {
                    for conn_id in conn_ids {
                        let downstream_terminal = self
                            .conn_target
                            .get(conn_id)
                            .and_then(|target| self.nodes.get(&target.node))
                            .is_some_and(NodeState::is_terminal);
                        if downstream_terminal
                            && let Some(ch) = self.channels.get_mut(conn_id)
                        {
                            *ch = Channel::for_kind(output.kind, self.config.channel_capacity);
                        }
                    }
                }
            }

            // Clear consumed_oneshots for the restarted node's input
            // connections so the fresh instance can receive oneshots.
            if let Some(entries) = self.node_inputs.get(node_id) {
                for (_, conn_id) in entries.iter() {
                    self.consumed_oneshots.remove(conn_id);
                }
            }

            self.mode.on_restarted(node_id);
            self.enqueue_node(node_id);
        }
        Ok(())
    }

}

// ------------------------------------------------------------------
// Conversion helpers
// ------------------------------------------------------------------

/// Converts an [`Activation`] to the WIT [`ActivationKind`].
///
/// Returns a [`NodeFault`] if a value cannot be serialized.
fn activation_to_wit(activation: &Activation) -> Result<ActivationKind, NodeFault> {
    use crate::engine::{val_to_bytes, DrainItemInfo, PortItemInfo};

    let serialize = |val: &Val, port: &PortName| -> Result<Vec<u8>, NodeFault> {
        val_to_bytes(val).map_err(|msg| NodeFault::WasmTrap {
            message: format!("serialization failed on port `{port}`: {msg}"),
        })
    };

    Ok(match activation {
        Activation::Sync => ActivationKind::Sync,
        Activation::DrainItem { port, item } => {
            ActivationKind::DrainItem(DrainItemInfo {
                port: port.to_string(),
                data: serialize(item, port)?,
            })
        }
        Activation::StreamItem { port, item } => {
            ActivationKind::StreamItem(PortItemInfo {
                port: port.to_string(),
                data: serialize(item, port)?,
            })
        }
        Activation::Event { port, payload } => {
            ActivationKind::Event(PortItemInfo {
                port: port.to_string(),
                data: serialize(payload, port)?,
            })
        }
        Activation::FutureResolved { port, value } => {
            ActivationKind::FutureResolved(PortItemInfo {
                port: port.to_string(),
                data: serialize(value, port)?,
            })
        }
        Activation::StreamClosed { port } => {
            ActivationKind::StreamClosed(port.to_string())
        }
    })
}

/// Converts a WIT [`WitActivationResult`] to our [`ActivationResult`].
fn wit_activation_result_to_ours(wit: WitActivationResult) -> ActivationResult {
    match wit {
        WitActivationResult::Continue => ActivationResult::Continue,
        WitActivationResult::Completed => ActivationResult::Completed,
    }
}

/// Converts a wasmtime error to a [`NodeFault`].
fn wasmtime_error_to_fault(err: wasmtime::Error) -> NodeFault {
    if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        match trap {
            wasmtime::Trap::OutOfFuel => return NodeFault::FuelExhausted,
            wasmtime::Trap::Interrupt => return NodeFault::EpochInterrupted,
            _ => {}
        }
    }
    NodeFault::WasmTrap {
        message: err.to_string(),
    }
}

// ------------------------------------------------------------------
// Free helpers
// ------------------------------------------------------------------

/// Extracts the port name from an [`OutputWrite`].
fn output_write_port(write: &OutputWrite) -> &PortName {
    match write {
        OutputWrite::Value { port, .. }
        | OutputWrite::Event { port, .. }
        | OutputWrite::StreamPush { port, .. }
        | OutputWrite::StreamClose { port }
        | OutputWrite::FutureResolve { port, .. } => port,
    }
}

/// Maps a [`ChannelError`] to a [`NodeFault`] for the given port.
fn channel_error_to_fault(port: &PortName, err: ChannelError) -> NodeFault {
    match err {
        ChannelError::Full(cap) => NodeFault::ChannelOverflow {
            port: port.clone(),
            capacity: cap,
        },
        ChannelError::AlreadyResolved => NodeFault::DoubleResolve {
            port: port.clone(),
        },
        ChannelError::Closed => NodeFault::WriteAfterClose {
            port: port.clone(),
        },
    }
}
