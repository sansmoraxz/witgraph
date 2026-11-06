//! What a host does with a hub, written once: writing and clearing inputs,
//! snapshots and restores. A host's plane says how to reach its hub
//! ([`HubPorts`]); what follows from it is on [`SchedulerCore`], so both hosts
//! keep the scheduler and the hub in step the same way.

use std::collections::BTreeMap;

use witgraph_ir::{
    FeedbackIdx, IndexVec, InputIdx, NodeId, OutputIdx, PortDirection, PortKind, PortName,
    PortTable,
};
use witgraph_traits::{
    BoxFuture, DataPlane, MaybeSend, PortValues, Returned, RuntimeError, Snapshot, SnapshotError,
};

use crate::control::SchedulerCore;

/// A port of the hub with a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSlot {
    /// A Value input.
    Input(InputIdx),
    /// A Value output.
    Output(OutputIdx),
    /// A feedback connection's buffered value.
    Feedback(FeedbackIdx),
}

/// How a host reaches a graph's hub. A failure of the guest in any of it faults
/// the graph, as anywhere else: a write reports no error for it.
pub trait HubPorts: DataPlane {
    /// A port's type.
    type Type;
    /// A value read from the hub, or parsed from a snapshot.
    type Value: Clone + MaybeSend;
    /// What the host wrote to an input, kept to write to the next instance.
    type Write: Clone + MaybeSend;

    /// Whether this host's writes reach its values at once, not through its
    /// server: they must then wait for what an interrupted tick left queued. A
    /// host that queues them gets that order for free.
    const WRITES_DIRECTLY: bool;

    /// The payload type of a Value input (for an optional port, the inner type).
    fn input_type(&self, input: InputIdx) -> Option<&Self::Type>;

    /// The type of a Value output.
    fn output_type(&self, output: OutputIdx) -> Option<&Self::Type>;

    /// Gives access to what the host wrote, by input.
    fn with_retained<R>(
        &self,
        f: impl FnOnce(&mut IndexVec<InputIdx, Option<Self::Write>>) -> R,
    ) -> R;

    /// Whether two writes are known to write the same value.
    fn same_write(old: &Self::Write, new: &Self::Write) -> bool;

    /// A value as a write.
    fn write_of(value: Self::Value) -> Self::Write;

    /// A write as WAVE text, or `None` if it has none.
    fn written_wave(write: &Self::Write) -> Option<Result<String, String>>;

    /// `text` as a value of type `ty`.
    fn parse_value(ty: &Self::Type, text: &str) -> Result<Self::Value, String>;

    /// A value as WAVE text.
    fn value_wave(value: &Self::Value) -> Result<String, String>;

    /// Whether an instance is up to ask. With none, what the host wrote is
    /// written to the next one as it is built.
    fn is_up(&self) -> bool;

    /// Writes `write` to `input`, then reports
    /// [`ControlKind::HostWrote`](witgraph_traits::ControlKind::HostWrote). It
    /// happens once the future is first polled, whoever waits for it after that.
    fn send_write(
        &self,
        input: InputIdx,
        write: Self::Write,
    ) -> BoxFuture<Result<(), RuntimeError>>;

    /// Clears `input`, then reports
    /// [`ControlKind::HostCleared`](witgraph_traits::ControlKind::HostCleared),
    /// as [`send_write`](Self::send_write) does.
    fn send_clear(&self, input: InputIdx) -> BoxFuture<Result<(), RuntimeError>>;

    /// Writes what a restore puts in a new instance, once it is built, then
    /// reports [`ControlKind::Restored`](witgraph_traits::ControlKind::Restored),
    /// as [`send_write`](Self::send_write) does.
    fn send_restore(
        &self,
        writes: Vec<(PortSlot, Self::Value)>,
    ) -> BoxFuture<Result<(), RuntimeError>>;

    /// The value at `slot`.
    fn read(&self, slot: PortSlot) -> BoxFuture<Result<Option<Self::Value>, String>>;
}

/// A `returned`, as the hub numbers things: not to be trusted yet.
#[derive(Debug, Clone, Copy)]
pub struct ReturnedNumbers<'a> {
    /// `outputs-changed`.
    pub outputs_changed: &'a [u32],
    /// `inputs-changed`.
    pub inputs_changed: &'a [u32],
    /// `inputs-recorded`.
    pub inputs_recorded: &'a [u32],
    /// `inputs-cleared`.
    pub inputs_cleared: &'a [u32],
    /// `feedback-buffered`.
    pub feedback_buffered: &'a [u32],
}

/// What a hub's `run` returned, as the scheduler needs it: the one place its
/// numbers are trusted, on any host, and nothing past here sees a raw one.
pub fn decode_returned(ports: &PortTable, raw: ReturnedNumbers<'_>) -> Result<Returned, String> {
    fn decoded<I: witgraph_ir::Idx>(numbers: &[u32], len: usize) -> Result<Vec<I>, String> {
        numbers
            .iter()
            .map(|at| {
                let at = *at as usize;
                if at >= len {
                    return Err(format!("the hub named port {at} of {len}"));
                }
                Ok(I::at(at))
            })
            .collect()
    }
    let inputs = ports.inputs().len();
    Ok(Returned {
        outputs_changed: decoded(raw.outputs_changed, ports.outputs().len())?,
        inputs_changed: decoded(raw.inputs_changed, inputs)?,
        inputs_recorded: decoded(raw.inputs_recorded, inputs)?,
        inputs_cleared: decoded(raw.inputs_cleared, inputs)?,
        feedback_buffered: decoded(raw.feedback_buffered, ports.feedback().len())?,
    })
}

/// That `node` exists and `port` is an output of it of a latched kind, and which.
pub fn latched_output(
    ports: &PortTable,
    node: &NodeId,
    port: &PortName,
) -> Result<(OutputIdx, PortKind), RuntimeError> {
    ports
        .node(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
    ports
        .output(node, port)
        .map(|output| (output, ports.outputs()[output].kind))
        .filter(|(_, kind)| kind.is_latched())
        .ok_or_else(|| not_a_value_output(node, port))
}

/// That `node` exists and `port` is a Value input of it, and which.
pub fn value_input(
    ports: &PortTable,
    node: &NodeId,
    port: &PortName,
) -> Result<InputIdx, RuntimeError> {
    ports
        .node(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
    ports
        .input(node, port)
        .filter(|input| ports.inputs()[*input].kind == PortKind::Value)
        .ok_or_else(|| RuntimeError::NotAValuePort {
            node: node.clone(),
            port: port.clone(),
            direction: PortDirection::Input,
        })
}

/// What asking `node.port` for a value it does not have is.
pub fn not_a_value_output(node: &NodeId, port: &PortName) -> RuntimeError {
    RuntimeError::NotAValuePort {
        node: node.clone(),
        port: port.clone(),
        direction: PortDirection::Output,
    }
}

/// `text` as a value of the type at `ty`, of `node.port`.
fn parse_snapshot_value<P: HubPorts>(
    node: &NodeId,
    port: &PortName,
    ty: Option<&P::Type>,
    direction: PortDirection,
    text: &str,
) -> Result<P::Value, RuntimeError> {
    let invalid = |message: String| RuntimeError::SnapshotValue {
        node: node.clone(),
        port: port.clone(),
        message,
    };
    let ty = ty.ok_or_else(|| invalid(format!("not a Value {direction} port")))?;
    P::parse_value(ty, text).map_err(invalid)
}

impl<P: HubPorts> SchedulerCore<P> {
    fn erased(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        self
    }

    /// Handles what the instance reported, and on a host that writes directly,
    /// serves what was asked of it first.
    fn caught_up(&mut self) {
        if P::WRITES_DIRECTLY {
            self.erased().serve_queued();
        } else {
            self.erased().drain();
        }
    }

    /// A Value input the host may write, and its type.
    pub fn host_input(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(InputIdx, &P::Type), RuntimeError> {
        let ports = self.ports();
        ports
            .node(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        let not_a_port = || RuntimeError::NotAValuePort {
            node: node.clone(),
            port: port.clone(),
            direction: PortDirection::Input,
        };
        let input = ports.input(node, port).ok_or_else(not_a_port)?;
        let ty = self.plane.input_type(input).ok_or_else(not_a_port)?;
        if let Some(connection) = self.connected(input) {
            return Err(RuntimeError::ConnectedInput {
                node: node.clone(),
                port: port.clone(),
                connection: connection.clone(),
            });
        }
        Ok((input, ty))
    }

    /// Writes `write` to `input`: the hub says whether that changed anything, and
    /// a stopped graph has it written when it next starts. Giving up on this
    /// leaves the write to finish, with no node starting before it has.
    pub async fn write_host_input(
        &mut self,
        input: InputIdx,
        write: P::Write,
    ) -> Result<(), RuntimeError> {
        self.caught_up();
        let same = self.plane.with_retained(|retained| {
            let same = retained[input]
                .as_ref()
                .is_some_and(|old| P::same_write(old, &write));
            retained[input] = Some(write.clone());
            same
        });
        if !self.plane.is_up() {
            if same {
                self.erased().host_kept(input);
            } else {
                self.erased().host_set(input);
            }
            return Ok(());
        }
        self.erased().host_write_started(input);
        let sent = self.plane.send_write(input, write);
        match sent.await {
            // Stopped first, it is kept for the next instance.
            Ok(()) | Err(RuntimeError::InstanceStopped) => {}
            Err(e) => return Err(e),
        }
        self.erased().drain();
        Ok(())
    }

    /// Clears an input the host wrote, as [`write_host_input`](Self::write_host_input)
    /// writes one.
    pub async fn clear_host_input(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(), RuntimeError> {
        self.caught_up();
        let (input, _) = self.host_input(node, port)?;
        let held = self
            .plane
            .with_retained(|retained| retained[input].take().is_some());
        if !self.plane.is_up() {
            if held {
                self.erased().host_cleared(input);
            }
            return Ok(());
        }
        self.erased().host_clear_started(input);
        let sent = self.plane.send_clear(input);
        match sent.await {
            Ok(()) | Err(RuntimeError::InstanceStopped) => {}
            Err(e) => return Err(e),
        }
        self.erased().drain();
        Ok(())
    }

    /// One Value port's value as WAVE text, if it has one.
    async fn read_wave(&mut self, slot: PortSlot) -> Result<Option<String>, String> {
        let read = self.plane.read(slot);
        let read = read.await?;
        read.as_ref().map(P::value_wave).transpose()
    }

    /// The Value ports among `entries` as WAVE text, by node and port.
    async fn wave_values<I: witgraph_ir::Idx>(
        &mut self,
        entries: &IndexVec<I, witgraph_ir::PortEntry>,
        slot: impl Fn(I) -> PortSlot,
    ) -> Result<PortValues, SnapshotError> {
        let mut values: PortValues = BTreeMap::new();
        for (index, entry) in entries.iter_enumerated() {
            if entry.kind != PortKind::Value {
                continue;
            }
            let at = || format!("`{}.{}`", entry.port.node, entry.port.port);
            let text = self
                .read_wave(slot(index))
                .await
                .map_err(|message| SnapshotError { at: at(), message })?;
            if let Some(text) = text {
                values
                    .entry(entry.port.node.clone())
                    .or_default()
                    .insert(entry.port.port.clone(), text);
            }
        }
        Ok(values)
    }

    /// Captures the graph's host-visible state: what the hub holds, pulled
    /// through its getters as WAVE text, and what is in flight or owed. A stopped
    /// graph holds only the inputs the host wrote.
    pub async fn snapshot_hub(&mut self) -> Result<Snapshot, SnapshotError> {
        self.caught_up();
        let mut snapshot = self.erased().snapshot_base();
        let ports = self.ports().clone();
        if !self.plane.is_up() {
            let written: Vec<(InputIdx, Option<Result<String, String>>)> =
                self.plane.with_retained(|retained| {
                    retained
                        .iter_enumerated()
                        .filter_map(|(input, write)| {
                            Some((input, P::written_wave(write.as_ref()?)))
                        })
                        .collect()
                });
            for (input, text) in written {
                let port = &ports.inputs()[input].port;
                let at = || format!("`{}.{}`", port.node, port.port);
                let text = text
                    .ok_or_else(|| SnapshotError {
                        at: at(),
                        message: "the graph is stopped, and a typed value has no text".into(),
                    })?
                    .map_err(|message| SnapshotError { at: at(), message })?;
                snapshot
                    .inputs
                    .entry(port.node.clone())
                    .or_default()
                    .insert(port.port.clone(), text);
            }
            return Ok(snapshot);
        }
        snapshot.inputs = self.wave_values(ports.inputs(), PortSlot::Input).await?;
        snapshot.outputs = self.wave_values(ports.outputs(), PortSlot::Output).await?;
        for (connection, entry) in ports.feedback().iter_enumerated() {
            if ports.outputs()[entry.from].kind != PortKind::Value {
                continue;
            }
            let id = &entry.id;
            let text = self
                .read_wave(PortSlot::Feedback(connection))
                .await
                .map_err(|message| SnapshotError {
                    at: format!("feedback `{id}`"),
                    message,
                })?;
            if let Some(text) = text {
                snapshot.feedback.insert(id.clone(), text);
            }
        }
        Ok(snapshot)
    }

    /// Replaces the graph's host-visible state with `snapshot`, on a new instance
    /// built now. The next tick re-runs what was in flight, on the latched
    /// inputs, and what was owed.
    ///
    /// The graph must have nothing in flight ([`RuntimeError::NotQuiescent`]) and
    /// exactly the snapshot's nodes (every one with a phase), components (by
    /// content hash), connections and links ([`RuntimeError::SnapshotMismatch`]).
    /// Every value is parsed against its port's type
    /// ([`RuntimeError::SnapshotValue`]); on any error nothing is changed. Node
    /// phases and [`Snapshot::quiescent`] are informational and not restored, and
    /// no guest state outlives a restore. An instance that cannot be built, or a
    /// hub failing as it is written, is a fault. The restore is all in place
    /// before the first wait, so giving up on it leaves it to finish, with
    /// nothing starting until it has.
    pub async fn restore_hub(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.erased().check_snapshot(snapshot)?;
        let ports = self.ports().clone();
        let mut inputs: Vec<(InputIdx, P::Value)> = Vec::new();
        for (node, values) in &snapshot.inputs {
            for (port, text) in values {
                let index = ports.input(node, port);
                let ty = index.and_then(|input| self.plane.input_type(input));
                let val = parse_snapshot_value::<P>(node, port, ty, PortDirection::Input, text)?;
                if let Some(index) = index {
                    inputs.push((index, val));
                }
            }
        }
        let mut writes: Vec<(PortSlot, P::Value)> = Vec::new();
        for (node, values) in &snapshot.outputs {
            for (port, text) in values {
                let index = ports.output(node, port);
                let ty = index.and_then(|output| self.plane.output_type(output));
                let val = parse_snapshot_value::<P>(node, port, ty, PortDirection::Output, text)?;
                if let Some(index) = index {
                    writes.push((PortSlot::Output(index), val));
                }
            }
        }
        let mut pending: Vec<FeedbackIdx> = Vec::new();
        for (connection, text) in &snapshot.feedback {
            let index =
                ports
                    .feedback_index(connection)
                    .ok_or_else(|| RuntimeError::SnapshotMismatch {
                        message: format!(
                            "`{connection}` is not a feedback connection of this graph"
                        ),
                    })?;
            let from = ports.feedback()[index].from;
            let source = &ports.outputs()[from].port;
            let val = parse_snapshot_value::<P>(
                &source.node,
                &source.port,
                self.plane.output_type(from),
                PortDirection::Output,
                text,
            )?;
            writes.push((PortSlot::Feedback(index), val));
            pending.push(index);
        }

        // Nothing fails from here on: what goes wrong in the instance is a fault.
        let host_written = self.erased().host_written_in(snapshot);
        self.plane.with_retained(|retained| {
            *retained = IndexVec::from_fn(ports.inputs().len(), |_| None);
            for (input, val) in &inputs {
                if host_written.contains(*input) {
                    retained[*input] = Some(P::write_of(val.clone()));
                }
            }
        });
        self.erased().restart_for_restore();
        let presence: Vec<InputIdx> = inputs.iter().map(|(input, _)| *input).collect();
        self.erased().apply_restore(&presence, &pending, snapshot);
        // The next instance is written what the host wrote as it is built.
        writes.extend(
            inputs
                .into_iter()
                .filter(|(input, _)| !host_written.contains(*input))
                .map(|(input, val)| (PortSlot::Input(input), val)),
        );
        self.erased().restore_writes_started();
        let sent = self.plane.send_restore(writes);
        match sent.await {
            Ok(()) | Err(RuntimeError::InstanceStopped) => {}
            Err(e) => return Err(e),
        }
        self.erased().drain();
        Ok(())
    }
}
