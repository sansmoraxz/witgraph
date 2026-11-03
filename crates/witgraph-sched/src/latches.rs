//! A data plane that holds values in the host: the latched value of every
//! port, beside an [`Executor`] that runs the nodes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::channel::mpsc::UnboundedSender;
use futures::stream::{FuturesUnordered, StreamExt};
use witgraph_ir::{
    CompiledGraph, FeedbackIdx, Idx, IndexVec, InputIdx, NodeId, NodeIdx, OutputIdx, PortDirection,
    PortKind, PortName, PortSet, PortTable,
};
use witgraph_traits::plan::{InputField, MemberPlan, OutputField, RunShape};
use witgraph_traits::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, Executor, Latched, NodeFault,
    OptionPayload, OptionPayloadFn, Outcome, PortValue, PortVersion, Returned, RuntimeError,
    Serving, boxed,
};

use crate::control::PortVersions;

/// The latched value of each port or connection of one kind that has one.
pub(crate) type Latches<I, V> = IndexVec<I, Option<Arc<V>>>;

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
    /// Inside a stream group: its reader runs with the group.
    Internal,
    /// Anything else, at once.
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
    /// The inputs the host wrote: all that survives an instance.
    host_written: PortSet<InputIdx>,
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
        let group_of: Vec<Option<usize>> = compiled.nodes().map(|n| n.group).collect();
        let mut out_edges: Vec<Vec<Edge>> = vec![Vec::new(); ports.outputs().len()];
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            let (Some(output), Some(input)) = (
                ports.output(&conn.from.node, &conn.from.port),
                ports.input(&conn.to.node, &conn.to.port),
            ) else {
                continue;
            };
            let same_group = group_of[resolved.from_node].is_some()
                && group_of[resolved.from_node] == group_of[resolved.to_node];
            let route = match (ports.feedback_index(&conn.id), same_group) {
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
        Self {
            inputs: IndexVec::from_fn(ports.inputs().len(), |_| None),
            outputs: IndexVec::from_fn(ports.outputs().len(), |_| None),
            feedback: IndexVec::from_fn(ports.feedback().len(), |_| None),
            host_written: PortSet::new(ports.inputs().len()),
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

    fn set_input(&mut self, port: InputIdx, val: Arc<V>) -> bool {
        if self.inputs[port].as_ref().is_some_and(|old| **old == *val) {
            return false;
        }
        self.inputs[port] = Some(val);
        true
    }

    /// Writes a value to an input for the host. Returns whether it changed.
    pub(crate) fn host_set(&mut self, port: InputIdx, val: Arc<V>) -> bool {
        self.host_written.insert(port);
        self.set_input(port, val)
    }

    /// Forgets an input's value for the host. Returns whether it had one.
    pub(crate) fn host_clear(&mut self, port: InputIdx) -> bool {
        self.host_written.remove(port);
        self.inputs[port].take().is_some()
    }

    /// Writes a delivered value into an input from outside its group: unwrapped
    /// first when the connection unwraps an option, where `none` clears the
    /// input.
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
                if self.inputs[port].take().is_some() {
                    InputWrite::Cleared
                } else {
                    InputWrite::Unchanged
                }
            }
        }
    }

    /// Latches a returned `run`'s values and routes them. Each value is shared,
    /// not copied, between the outputs, the inputs it reaches and buffered
    /// feedback. Returned too: what nothing reads, to close.
    fn deliver(&mut self, member: &MemberPlan, values: Vec<(String, V)>) -> (Returned, Vec<V>) {
        let mut returned = Returned::default();
        let mut unread = Vec::new();
        for (port, val) in values {
            let port = PortName::from(port);
            let Some(OutputField {
                index: output,
                kind,
            }) = member.outputs.get(&port).copied()
            else {
                continue;
            };
            if kind.is_async() && self.out_edges[output].is_empty() {
                unread.push(val);
                continue;
            }
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
                    Route::Internal => match self.delivered(&val, unwrap_option) {
                        Some(delivered) => {
                            self.inputs[to] = Some(delivered);
                            returned.inputs_recorded.push(to);
                        }
                        None => {
                            if self.inputs[to].take().is_some() {
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
            if kind.is_latched() {
                self.outputs[output] = Some(val);
                returned.outputs_changed.push(output);
            }
        }
        (returned, unread)
    }

    /// What `member` starts with: its inputs' latched values. A stream or
    /// future moves to its reader.
    fn args(&mut self, member: &MemberPlan) -> Vec<Option<V>> {
        member
            .inputs
            .iter()
            .flatten()
            .map(|field| {
                let value = if field.kind.is_async() {
                    self.inputs[field.index].take()
                } else {
                    self.inputs[field.index].clone()
                };
                value.map(|value| (*value).clone())
            })
            .collect()
    }

    fn latch_feedback(&mut self, connection: FeedbackIdx) -> Latched {
        let Some(val) = self.feedback[connection].take() else {
            return Latched::Unchanged;
        };
        let entry = &self.ports.feedback()[connection];
        let (to, unwrap_option) = (entry.to, entry.unwraps_option);
        match self.write_input(to, &val, unwrap_option) {
            InputWrite::Unchanged => Latched::Unchanged,
            InputWrite::Changed => Latched::Changed,
            InputWrite::Cleared => Latched::Cleared,
        }
    }

    /// Puts a snapshot's values in place. What `host` says of an input is
    /// whether it survives a new instance as the host's.
    pub(crate) fn restore(
        &mut self,
        inputs: Latches<InputIdx, V>,
        outputs: Latches<OutputIdx, V>,
        feedback: Latches<FeedbackIdx, V>,
        host: impl Fn(InputIdx) -> bool,
    ) {
        self.host_written.clear();
        for (input, value) in inputs.iter_enumerated() {
            if value.is_some() && host(input) {
                self.host_written.insert(input);
            }
        }
        self.inputs = inputs;
        self.outputs = outputs;
        self.feedback = feedback;
    }

    /// The instance is gone: all that survives is what the host wrote.
    fn reset(&mut self) {
        for (input, value) in self.inputs.iter_enumerated_mut() {
            if !self.host_written.contains(input) {
                *value = None;
            }
        }
        self.outputs = IndexVec::from_fn(self.outputs.len(), |_| None);
        self.feedback = IndexVec::from_fn(self.feedback.len(), |_| None);
    }
}

/// `latches`, whichever panic may have poisoned them: a panic in a guest call
/// leaves them as consistent as they were.
fn lock<V: Clone>(latches: &Mutex<HostLatches<V>>) -> MutexGuard<'_, HostLatches<V>> {
    latches.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How to call each node, from the shapes of their `run`s.
pub(crate) fn plan_members(
    compiled: &CompiledGraph,
    ports: &PortTable,
    shapes: &HashMap<NodeId, RunShape>,
) -> Result<IndexVec<NodeIdx, Arc<MemberPlan>>, RuntimeError> {
    (0..ports.node_count())
        .map(|at| {
            let index = NodeIdx::from_position(at);
            let node = ports.node_id(index);
            let unknown = || RuntimeError::UnknownNode { node: node.clone() };
            let contract = compiled.contract_for(node).ok_or_else(unknown)?;
            let shape = shapes.get(node).ok_or_else(unknown)?;
            // A shape names the contract's inputs: fields are as it lists them.
            let inputs = shape
                .inputs
                .as_ref()
                .map(|fields| {
                    fields
                        .iter()
                        .map(|name| {
                            let port = contract
                                .inputs
                                .iter()
                                .find(|p| p.name.as_str() == name)
                                .ok_or_else(|| RuntimeError::InvalidConfig {
                                    message: format!(
                                        "`{node}`'s `run` takes `{name}`, which its contract has no input for"
                                    ),
                                })?;
                            let index = ports.input(node, &port.name).ok_or_else(unknown)?;
                            Ok(InputField {
                                name: port.name.clone(),
                                port: witgraph_ir::PortRef::new(node.clone(), port.name.clone()),
                                index,
                                kind: port.kind,
                                optional: port.optional,
                            })
                        })
                        .collect::<Result<Vec<_>, RuntimeError>>()
                })
                .transpose()?;
            let outputs = contract
                .outputs
                .iter()
                .filter_map(|port| {
                    let index = ports.output(node, &port.name)?;
                    Some((
                        port.name.clone(),
                        OutputField {
                            index,
                            kind: port.kind,
                        },
                    ))
                })
                .collect();
            Ok(Arc::new(MemberPlan {
                node: node.clone(),
                index,
                inputs,
                has_result: shape.has_result,
                outputs,
            }))
        })
        .collect()
}

/// An [`Executor`] over host-held latches.
pub(crate) struct HostPlane<X: Executor> {
    pub(crate) latches: Arc<Mutex<HostLatches<X::Value>>>,
    pub(crate) executor: Arc<X>,
    members: Arc<IndexVec<NodeIdx, Arc<MemberPlan>>>,
    /// Whether the next instance is the first, whose guest state is fresh.
    fresh: Mutex<bool>,
}

impl<X: Executor> HostPlane<X> {
    pub(crate) fn new(
        executor: X,
        compiled: &CompiledGraph,
        ports: &Arc<PortTable>,
        members: IndexVec<NodeIdx, Arc<MemberPlan>>,
    ) -> Self {
        let latches = HostLatches::new(compiled, ports, X::option_payload);
        Self {
            latches: Arc::new(Mutex::new(latches)),
            executor: Arc::new(executor),
            members: Arc::new(members),
            fresh: Mutex::new(true),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, HostLatches<X::Value>> {
        lock(&self.latches)
    }
}

/// Tells the executor, if an instance ends with calls in flight, that those are
/// abandoned.
struct Abandon<'a, X: Executor> {
    executor: &'a X,
    members: &'a IndexVec<NodeIdx, Arc<MemberPlan>>,
    in_flight: bool,
}

impl<X: Executor> Drop for Abandon<'_, X> {
    fn drop(&mut self) {
        if self.in_flight {
            let nodes: Vec<NodeId> = self.members.iter().map(|m| m.node.clone()).collect();
            self.executor.abandon(&nodes);
        }
    }
}

/// Which node's `run` ended, with what.
type CallDone<V> = (
    NodeIdx,
    Result<Vec<(String, V)>, (NodeFault, Option<NodeId>)>,
);

/// Serves one instance.
async fn serve_host<X: Executor>(
    executor: Arc<X>,
    latches: Arc<Mutex<HostLatches<X::Value>>>,
    members: Arc<IndexVec<NodeIdx, Arc<MemberPlan>>>,
    fresh: bool,
    serving: Serving,
) -> Outcome {
    let Serving {
        incarnation,
        mut commands,
        events,
        ..
    } = serving;
    let mut abandon = Abandon {
        executor: &*executor,
        members: &members,
        in_flight: false,
    };
    let send = |events: &UnboundedSender<ControlEvent>, kind| {
        // The receiver lives as long as the scheduler; a send can only fail
        // while it is being dropped.
        let _ = events.unbounded_send(ControlEvent { incarnation, kind });
    };
    if !fresh {
        abandon.in_flight = true;
        let rebuilt = executor.rebuild().await;
        // Every rebuild has settled: nothing is left in flight.
        abandon.in_flight = false;
        rebuilt?;
        send(&events, ControlKind::Built);
    }
    let mut calls = FuturesUnordered::new();
    let mut closed = false;
    loop {
        futures::select_biased! {
            done = calls.select_next_some() => {
                let (node, result): CallDone<X::Value> = done;
                abandon.in_flight = !calls.is_empty();
                let values = result?;
                let (returned, unread) = lock(&latches).deliver(&members[node], values);
                for value in unread {
                    executor.close(value);
                }
                send(&events, ControlKind::RunReturned { node, returned });
            }
            command = commands.next() => match command {
                Some(Command::Run(node)) => {
                    let member = members[node].clone();
                    let args = lock(&latches).args(&member);
                    // Checked before anything started: a required input only
                    // lacks a value if its stream's producer did not deliver.
                    if let Some(field) = member
                        .inputs
                        .iter()
                        .flatten()
                        .zip(&args)
                        .find(|(field, arg)| !field.optional && arg.is_none())
                        .map(|(field, _)| field)
                    {
                        let message = format!(
                            "no value for required input `{}.{}`",
                            member.node, field.name
                        );
                        return Err((NodeFault::WasmTrap { message }, Some(member.node.clone())));
                    }
                    let call = executor.call(member, args);
                    calls.push(async move { (node, call.await) });
                    abandon.in_flight = true;
                }
                Some(Command::LatchFeedback(connection)) => {
                    let latched = lock(&latches).latch_feedback(connection);
                    send(&events, ControlKind::Latched { connection, latched });
                }
                Some(Command::DropFeedback(connection)) => {
                    lock(&latches).feedback[connection] = None;
                }
                Some(Command::AwaitQuiet(at)) => send(&events, ControlKind::Quiet(at)),
                None => closed = true,
            },
            complete => break,
        }
        if closed && calls.is_empty() {
            break;
        }
    }
    Ok(())
}

impl<X: Executor> DataPlane for HostPlane<X> {
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome> {
        let fresh = {
            let mut fresh = self.fresh.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *fresh)
        };
        boxed(serve_host(
            self.executor.clone(),
            self.latches.clone(),
            self.members.clone(),
            fresh,
            serving,
        ))
    }

    fn stopped(&self) {
        self.lock().reset();
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

    /// Which value of a Value or handle output port is latched.
    pub fn output_version(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<PortVersion, RuntimeError> {
        self.latched_output(node, port)
            .map(|output| self.versions.get(output))
    }
}
