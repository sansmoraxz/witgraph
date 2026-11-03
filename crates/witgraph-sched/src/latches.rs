//! A data plane that holds values in the host: the latched value of every
//! port, shared with the generations that write them.

use std::any::Any;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::FutureExt;
use futures::channel::mpsc::UnboundedSender;
use witgraph_ir::{
    CompiledGraph, FeedbackIdx, Idx, IndexVec, InputIdx, NodeId, OutputIdx, PortDirection,
    PortKind, PortName, PortTable,
};

use crate::control::{ControlEvent, ControlKind, PortVersions};
use crate::error::{NodeFault, RuntimeError};
use crate::executor::{
    DataPlane, EventSink, Executor, Generation, GenerationFuture, IslandEvent, IslandEventKind,
    Latched, LiveIsland, OptionPayload, OptionPayloadFn, Outcome, PortValue, Returned,
    StartGeneration,
};
use crate::plan::{IslandPlan, MemberPlan};
use witgraph_traits::PortVersion;

/// The latched value of each port or connection of one kind that has one.
pub(crate) type Latches<I, V> = IndexVec<I, Option<Arc<V>>>;

/// The external Value inputs of a generation: per member in plan order,
/// per field of its `inputs` record (`None` for a field with no value, or
/// one a member of the island writes). Shared with the latched values they
/// were read from.
pub(crate) type StartInputs<V> = Vec<Vec<Option<Arc<V>>>>;

/// An outgoing connection of an output port.
#[derive(Debug, Clone, Copy)]
struct Edge {
    to: InputIdx,
    route: Route,
    /// An `option<T>` output feeding an optional input of payload `T`
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    unwrap_option: bool,
}

/// How a delivered value travels along an [`Edge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Buffered until the end-of-iteration latch.
    Feedback(FeedbackIdx),
    /// To a member of the producer's own island: part of the generation.
    Internal,
    /// To another island, at once.
    External,
}

/// What writing an input did.
enum InputWrite {
    Unchanged,
    Changed,
    Cleared,
}

/// Every latched value of a graph.
pub(crate) struct HostLatches<V: Clone> {
    pub(crate) inputs: Latches<InputIdx, V>,
    pub(crate) outputs: Latches<OutputIdx, V>,
    /// Feedback writes waiting for the end-of-iteration latch: the value as
    /// the source produced it, by connection.
    pub(crate) feedback: Latches<FeedbackIdx, V>,
    /// Per island, the external inputs of its generation in flight.
    pub(crate) started: Vec<Option<StartInputs<V>>>,
    /// Per island, the inputs a restored generation is to replay.
    pub(crate) replays: Vec<Option<StartInputs<V>>>,
    out_edges: IndexVec<OutputIdx, Box<[Edge]>>,
    ports: Arc<PortTable>,
    option_payload: OptionPayloadFn<V>,
}

impl<V: PortValue> HostLatches<V> {
    fn new(
        compiled: &CompiledGraph,
        ports: &Arc<PortTable>,
        option_payload: OptionPayloadFn<V>,
    ) -> Self {
        let island_of: Vec<usize> = compiled.nodes().map(|n| n.island).collect();
        let mut out_edges: Vec<Vec<Edge>> = vec![Vec::new(); ports.outputs().len()];
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (Some(output), Some(input)) = (
                ports.output(&conn.from.node, &conn.from.port),
                ports.input(&conn.to.node, &conn.to.port),
            ) else {
                continue;
            };
            let same_island = island_of[resolved.from_node] == island_of[resolved.to_node];
            let route = match (ports.feedback_index(&conn.id), same_island) {
                (Some(feedback), _) => Route::Feedback(feedback),
                (None, true) => Route::Internal,
                (None, false) => Route::External,
            };
            out_edges[output.position()].push(Edge {
                to: input,
                route,
                unwrap_option: resolved.unwraps_option,
            });
        }
        let islands = compiled.islands().len();
        Self {
            inputs: IndexVec::from_fn(ports.inputs().len(), |_| None),
            outputs: IndexVec::from_fn(ports.outputs().len(), |_| None),
            feedback: IndexVec::from_fn(ports.feedback().len(), |_| None),
            started: (0..islands).map(|_| None).collect(),
            replays: (0..islands).map(|_| None).collect(),
            out_edges: out_edges.into_iter().map(Vec::into_boxed_slice).collect(),
            ports: ports.clone(),
            option_payload,
        }
    }

    /// What a connection delivers to its input: the value itself, or, when
    /// the connection unwraps an option, its payload.
    fn delivered(&self, val: &Arc<V>, unwrap_option: bool) -> Option<Arc<V>> {
        if !unwrap_option {
            return Some(val.clone());
        }
        match (self.option_payload)(val) {
            // Not an option: delivered itself, shared rather than copied.
            OptionPayload::NotOption => Some(val.clone()),
            OptionPayload::Some(payload) => Some(Arc::new(payload.into_owned())),
            OptionPayload::None => None,
        }
    }

    /// Writes a value to an input from outside its island. Returns whether
    /// the value changed.
    pub(crate) fn set_input(&mut self, port: InputIdx, val: Arc<V>) -> bool {
        if self.inputs[port].as_ref().is_some_and(|old| **old == *val) {
            return false;
        }
        self.inputs[port] = Some(val);
        true
    }

    /// Forgets an input's value. Returns whether it had one.
    pub(crate) fn clear_input(&mut self, port: InputIdx) -> bool {
        self.inputs[port].take().is_some()
    }

    /// Writes a delivered value into an input from outside its island:
    /// unwrapped first when the connection unwraps an option, where `none`
    /// clears the input.
    fn write_input(&mut self, port: InputIdx, val: &Arc<V>, unwrap_option: bool) -> InputWrite {
        match self.delivered(val, unwrap_option) {
            Some(val) => {
                if self.set_input(port, val) {
                    InputWrite::Changed
                } else {
                    InputWrite::Unchanged
                }
            }
            None => {
                if self.clear_input(port) {
                    InputWrite::Cleared
                } else {
                    InputWrite::Unchanged
                }
            }
        }
    }

    /// Latches a returned `run`'s values and routes them. Each value is
    /// shared, not copied, between the outputs, the inputs it reaches and
    /// buffered feedback. Guest values are already canonical.
    fn deliver(&mut self, member: &MemberPlan, values: Vec<(PortName, V)>) -> Returned {
        let mut returned = Returned::default();
        for (port, val) in values {
            let Some(output) = member.outputs.get(&port).map(|output| output.index) else {
                continue;
            };
            let val = Arc::new(val);
            for at in 0..self.out_edges[output].len() {
                let Edge {
                    to,
                    route,
                    unwrap_option,
                } = self.out_edges[output][at];
                match route {
                    Route::Feedback(conn) => {
                        self.feedback[conn] = Some(val.clone());
                        returned.feedback_buffered.push(conn);
                    }
                    // Consumed inside this generation; recorded, not a
                    // change.
                    Route::Internal => match self.delivered(&val, unwrap_option) {
                        Some(delivered) => {
                            self.inputs[to] = Some(delivered);
                            returned.inputs_recorded.push(to);
                        }
                        None => {
                            if self.clear_input(to) {
                                returned.inputs_cleared.push(to);
                            }
                        }
                    },
                    Route::External => match self.write_input(to, &val, unwrap_option) {
                        InputWrite::Changed => returned.inputs_changed.push(to),
                        InputWrite::Cleared => returned.inputs_cleared.push(to),
                        InputWrite::Unchanged => {}
                    },
                }
            }
            self.outputs[output] = Some(val);
            returned.outputs_changed.push(output);
        }
        returned
    }

    /// The latched external Value inputs of each member, per field.
    fn external_inputs(&self, plan: &IslandPlan) -> StartInputs<V> {
        plan.members
            .iter()
            .map(|member| {
                let mut fields = vec![None; member.field_count()];
                for (field, input) in member.external_values() {
                    fields[field] = self.inputs[input.index].clone();
                }
                fields
            })
            .collect()
    }
}

/// `latches`, whichever panic may have poisoned them: a panic in a guest call
/// leaves them as consistent as they were.
fn lock<V: Clone>(latches: &Mutex<HostLatches<V>>) -> MutexGuard<'_, HostLatches<V>> {
    latches.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Latches what generations return, then tells the scheduler what changed.
pub(crate) struct LatchSink<V: Clone> {
    latches: Arc<Mutex<HostLatches<V>>>,
    plans: Vec<Arc<IslandPlan>>,
    events: UnboundedSender<ControlEvent>,
}

impl<V: PortValue> EventSink<V> for LatchSink<V> {
    fn send(&self, event: IslandEvent<V>) {
        let IslandEvent {
            island,
            generation,
            kind,
        } = event;
        let kind = match kind {
            IslandEventKind::RunStarted { member } => ControlKind::RunStarted { member },
            IslandEventKind::Rebuilt => ControlKind::Rebuilt,
            IslandEventKind::StreamItems {
                member,
                port,
                count,
            } => ControlKind::StreamItems {
                member,
                port,
                count,
            },
            IslandEventKind::RunReturned { member, values } => {
                let plan = self.plans.get(island).and_then(|p| p.members.get(member));
                let returned = plan
                    .map(|plan| lock(&self.latches).deliver(plan, values))
                    .unwrap_or_default();
                ControlKind::RunReturned { member, returned }
            }
        };
        // The receiver lives as long as the scheduler; a send can only fail
        // while it is being dropped.
        let _ = self.events.unbounded_send(ControlEvent {
            island,
            generation,
            kind,
        });
    }
}

/// An [`Executor`] over host-held latches.
pub(crate) struct HostPlane<X: Executor> {
    pub(crate) latches: Arc<Mutex<HostLatches<X::Value>>>,
    pub(crate) sink: Arc<LatchSink<X::Value>>,
    pub(crate) executor: X,
}

impl<X: Executor> HostPlane<X> {
    pub(crate) fn new(
        executor: X,
        compiled: &CompiledGraph,
        ports: &Arc<PortTable>,
        plans: &[Arc<IslandPlan>],
        events: UnboundedSender<ControlEvent>,
    ) -> Self {
        let latches = HostLatches::new(compiled, ports, X::option_payload);
        let latches = Arc::new(Mutex::new(latches));
        let sink = Arc::new(LatchSink {
            latches: latches.clone(),
            plans: plans.to_vec(),
            events,
        });
        Self {
            latches,
            sink,
            executor,
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, HostLatches<X::Value>> {
        lock(&self.latches)
    }

    /// The generation `start` asks for, recording what it starts with.
    fn generation(&self, start: StartGeneration) -> Generation<X::Value> {
        let StartGeneration {
            plan,
            number,
            stream_items,
            replay,
        } = start;
        let mut latches = self.lock();
        let replayed = replay.then(|| latches.replays[plan.index].take());
        let inputs = match replayed.flatten() {
            Some(inputs) => inputs,
            None => latches.external_inputs(&plan),
        };
        let external = inputs
            .iter()
            .map(|fields| fields.iter().map(|v| v.as_deref().cloned()).collect())
            .collect();
        latches.started[plan.index] = Some(inputs);
        drop(latches);
        Generation {
            plan,
            external,
            number,
            stream_items,
            events: self.sink.clone(),
        }
    }
}

/// The generation future with its live island made opaque again.
fn opaque<L: LiveIsland>(future: GenerationFuture<L>) -> GenerationFuture<Box<dyn LiveIsland>> {
    let future = future
        .map(|outcome: Outcome<L>| outcome.map(|island| Box::new(island) as Box<dyn LiveIsland>));
    #[cfg(not(target_family = "wasm"))]
    return future.boxed();
    #[cfg(target_family = "wasm")]
    return future.boxed_local();
}

impl<X: Executor> DataPlane for HostPlane<X> {
    fn run(
        &self,
        island: Box<dyn LiveIsland>,
        start: StartGeneration,
    ) -> GenerationFuture<Box<dyn LiveIsland>> {
        let island: Box<dyn Any> = island;
        match island.downcast::<X::Island>() {
            Ok(island) => opaque(self.executor.run(*island, self.generation(start))),
            // The scheduler only ever hands back what this plane gave it.
            Err(_) => {
                let fault = NodeFault::Restart {
                    message: "a live island of another executor".into(),
                };
                opaque::<X::Island>(Box::pin(async move { Err((fault, None)) }))
            }
        }
    }

    fn rebuild_and_run(&self, start: StartGeneration) -> GenerationFuture<Box<dyn LiveIsland>> {
        opaque(self.executor.rebuild_and_run(self.generation(start)))
    }

    fn generation_ended(&self, island: usize) {
        self.lock().started[island] = None;
    }

    fn latch_feedback(&self, connection: FeedbackIdx) -> Latched {
        let mut latches = self.lock();
        let Some(val) = latches.feedback[connection].take() else {
            return Latched::Unchanged;
        };
        let entry = &latches.ports.feedback()[connection];
        let (to, unwrap_option) = (entry.to, entry.unwraps_option);
        match latches.write_input(to, &val, unwrap_option) {
            InputWrite::Unchanged => Latched::Unchanged,
            InputWrite::Changed => Latched::Changed,
            InputWrite::Cleared => Latched::Cleared,
        }
    }

    fn drop_feedback(&self, connection: FeedbackIdx) {
        self.lock().feedback[connection] = None;
    }
}

/// Reads a graph's latched outputs at any time, while a tick holds the
/// scheduler too.
pub struct LatchReader<V: Clone> {
    pub(crate) latches: Arc<Mutex<HostLatches<V>>>,
    pub(crate) ports: Arc<PortTable>,
    pub(crate) versions: PortVersions,
}

impl<V: Clone> Clone for LatchReader<V> {
    fn clone(&self) -> Self {
        Self {
            latches: self.latches.clone(),
            ports: self.ports.clone(),
            versions: self.versions.clone(),
        }
    }
}

impl<V: PortValue> LatchReader<V> {
    /// Checks that `node` exists and `port` is a Value or handle output of
    /// it: one whose values the host latches.
    fn latched_output(&self, node: &NodeId, port: &PortName) -> Result<OutputIdx, RuntimeError> {
        self.ports
            .node(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        self.ports
            .output(node, port)
            .filter(|output| self.ports.outputs()[*output].kind.is_latched())
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Output,
            })
    }

    /// The kind of a Value or handle output port.
    pub fn output_kind(&self, node: &NodeId, port: &PortName) -> Result<PortKind, RuntimeError> {
        self.latched_output(node, port)
            .map(|output| self.ports.outputs()[output].kind)
    }

    /// The latched value of a Value or handle output port: what the node's
    /// `run` returned last, or `None` if it has not run yet.
    pub fn read_output(&self, node: &NodeId, port: &PortName) -> Result<Option<V>, RuntimeError> {
        let output = self.latched_output(node, port)?;
        Ok(lock(&self.latches).outputs[output]
            .as_ref()
            .map(|val| (**val).clone()))
    }

    /// Which value of a Value or handle output port is latched: it moves on
    /// each time the node's `run` writes the port, so a host can tell it has
    /// seen the latest without reading it.
    pub fn output_version(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<PortVersion, RuntimeError> {
        self.latched_output(node, port)
            .map(|output| self.versions.get(output))
    }
}
