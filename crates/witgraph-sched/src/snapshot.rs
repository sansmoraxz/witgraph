//! Snapshots of the host-visible state, and restoring them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use witgraph_ir::{
    ComponentRef, Connection, ConnectionId, FeedbackIdx, Idx, IndexVec, Link, NodeId,
    PortDirection, PortEntry, PortKind, PortName, PortRef, PortSet,
};

use crate::error::RuntimeError;
use crate::executor::Executor;
use crate::island::{Replay, StartInputs, StopCause};
use crate::mode::RuntimeMode;
use crate::node::NodePhase;
use crate::scheduler::{Latches, Scheduler};

/// WAVE text per port, keyed by node id then port name.
pub type PortValues = BTreeMap<NodeId, BTreeMap<PortName, String>>;

/// The host-visible state of a runtime graph at one point in time, for
/// point-in-time restores and debugging. Produced by
/// [`Scheduler::snapshot`] (at any time), applied by
/// [`Scheduler::restore`].
///
/// Values are WAVE text, parsed against the port types on restore. Only
/// what the host sees is captured: latched Values, pending feedback, node
/// phases, and the inputs each in-flight or queued generation starts with.
/// Guest memory (including suspended tasks) is not captured either.
///
/// Snapshots do not store stream or future contents. Restoring re-runs any
/// generation that was in flight, which recreates its streams from the
/// recorded inputs; the result matches the original only if the guests are
/// deterministic.
///
/// Nor do they store handles, which are the host's and not text: a latched
/// handle is left out, and the island that produced it is recorded as
/// queued, so after a restore it runs again and makes a new one. A
/// generation in flight that started with a handle is recorded as queued
/// instead of replayed: it runs once its producers have delivered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Each node's resolved component (id and content hash). A snapshot
    /// restores only onto a graph with exactly these nodes and components.
    pub nodes: BTreeMap<NodeId, ComponentRef>,
    /// The graph's connections, sorted by id. A snapshot restores only onto
    /// a graph wired exactly the same way.
    pub connections: Vec<Connection>,
    /// The graph's links, sorted by id. A snapshot restores only onto a
    /// graph whose nodes are linked to the same providers, by ref. Like
    /// nodes, providers are named, not their bytes, and a provider's content
    /// hash is not checked against its bytes (it has no contract to hash):
    /// a pinned ref only selects the key its bytes are loaded under. Keep
    /// the bytes behind a provider's key unchanged for a restore to replay
    /// the same code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Whether no generation was in flight when the snapshot was taken. A
    /// replay owed after a restore is not in flight, so it is listed in
    /// [`islands`](Self::islands) while this is `true`.
    pub quiescent: bool,
    /// Every node's phase when the snapshot was taken. Informational:
    /// [`Scheduler::restore`] does not re-fault or re-cancel islands.
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
    /// The island's members, sorted. [`Scheduler::restore`] matches
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

/// What a snapshot says an island owes: a replay, and whether a run on the
/// latched inputs is queued.
type OwedWork<V> = (Option<Replay<V>>, bool);

/// Why a snapshot could not be taken: a value it holds could not be written
/// as WAVE text ([`Executor::to_wave`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{at}: {message}")]
pub struct SnapshotError {
    /// Which value: an output or input (`` `node.port` ``), a feedback
    /// connection, or an input a generation started with.
    pub at: String,
    /// Why it could not be written.
    pub message: String,
}

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
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
    /// Handles are left out, and the islands that produced them owe a run
    /// (see [`Snapshot`]).
    ///
    /// Fails, naming it, on a value the executor cannot write as WAVE text.
    pub fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        let wave = |val: &X::Value, at: &dyn Fn() -> String| self.wave(val, at);
        // A handle is the host's, not text: it is left out, and the island
        // that produced it owes a run, which makes a new one.
        let producers: BTreeSet<usize> = self
            .outputs
            .iter_enumerated()
            .filter(|(output, val)| {
                val.is_some() && self.ports.outputs()[*output].kind == PortKind::Handle
            })
            .map(|(output, _)| self.island_of_node[self.ports.outputs()[output].node])
            .collect();
        let mut feedback = BTreeMap::new();
        for (conn, val) in self.feedback.iter_enumerated() {
            let entry = &self.ports.feedback()[conn];
            if let Some(val) = val
                && self.ports.outputs()[entry.from].kind != PortKind::Handle
            {
                let id = &entry.id;
                feedback.insert(id.clone(), wave(val, &|| format!("feedback `{id}`"))?);
            }
        }
        let mut islands = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            // In flight, or a restored replay not started yet: its inputs,
            // and what it must not feed back.
            let started = match (slot.state.started_with(), slot.owed.replay()) {
                (Some(inputs), _) => Some((inputs, &slot.stale_feedback)),
                (None, Some(replay)) => Some((&replay.inputs, &replay.stale)),
                (None, None) => None,
            };
            // One that started with a handle cannot be replayed: it owes a
            // run instead, on the handles its producers make anew.
            let with_handle = started.is_some_and(|(inputs, _)| {
                slot.plan
                    .members
                    .iter()
                    .zip(inputs)
                    .any(|(member, fields)| {
                        member
                            .inputs
                            .iter()
                            .flatten()
                            .zip(fields)
                            .any(|(field, val)| field.kind == PortKind::Handle && val.is_some())
                    })
            });
            let started = started.filter(|_| !with_handle);
            let running = started.map(|(inputs, _)| {
                let mut by_member = PortValues::new();
                for (member, fields) in slot.plan.members.iter().zip(inputs) {
                    let entry = by_member.entry(member.node.clone()).or_default();
                    for (field, val) in member.inputs.iter().flatten().zip(fields) {
                        if let Some(val) = val {
                            let at =
                                || format!("the started input `{}.{}`", member.node, field.name);
                            entry.insert(field.name.clone(), wave(val, &at)?);
                        }
                    }
                }
                Ok::<_, SnapshotError>(by_member)
            });
            let running = running.transpose()?;
            let stale_feedback = started
                .map(|(_, stale)| {
                    stale
                        .iter()
                        .map(|conn| self.ports.feedback()[conn].id.clone())
                        .collect()
                })
                .unwrap_or_default();
            let queued = slot.owed.has_latched() || with_handle || producers.contains(&index);
            if running.is_some() || queued {
                islands.push(IslandSnapshot {
                    members: slot.plan.sorted_members(),
                    running,
                    queued,
                    stale_feedback,
                });
            }
        }
        Ok(Snapshot {
            nodes: self.components.clone(),
            connections: self.connections.clone(),
            links: self.links.clone(),
            quiescent: self.running == 0,
            phases: self
                .slots
                .iter()
                .flat_map(|slot| {
                    let phase = slot.state.node_phase();
                    slot.plan
                        .members
                        .iter()
                        .map(move |m| (m.node.clone(), phase))
                })
                .collect(),
            inputs: self.latched_values(self.ports.inputs(), &self.inputs)?,
            outputs: self.latched_values(self.ports.outputs(), &self.outputs)?,
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
    /// The graph must have nothing in flight ([`RuntimeError::NotQuiescent`])
    /// — freshly loaded, after [`shutdown`](Self::shutdown), or right after
    /// every running island was [`cancel`](Self::cancel)led — and exactly
    /// the snapshot's nodes (every one with a phase), components (by
    /// content hash), connections and links
    /// ([`RuntimeError::SnapshotMismatch`],
    /// as for an island that does not exist, is listed twice, names a node
    /// outside it, or lists stale feedback that is not a feedback
    /// connection out of it). Every value is parsed against its port's type
    /// ([`RuntimeError::SnapshotValue`]); on any error nothing is changed.
    /// Node phases and [`Snapshot::quiescent`] are informational and not
    /// restored, and no guest state outlives a restore: every island drops
    /// its Store (its nodes read as [`Pending`](crate::NodePhase::Pending))
    /// and is rebuilt when its replay or next generation starts. A replay
    /// must bring every required external Value input of its island, and
    /// only external Value inputs ([`RuntimeError::SnapshotValue`]). A
    /// replay runs before its own island's queued run, but like any start
    /// it waits for what is upstream of the island and for resources, so
    /// other islands may start first. Its feedback into an input the host
    /// writes after the restore is stale, and so is its feedback the
    /// snapshot recorded as stale ([`IslandSnapshot::stale_feedback`]).
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.drain_events();
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
        let inputs = self.parse_values(
            &snapshot.inputs,
            PortDirection::Input,
            self.ports.inputs().len(),
            |node, port| self.ports.input(node, port),
        )?;
        let outputs = self.parse_values(
            &snapshot.outputs,
            PortDirection::Output,
            self.ports.outputs().len(),
            |node, port| self.ports.output(node, port),
        )?;
        let mut feedback = IndexVec::from_fn(self.feedback.len(), |_| None);
        for (conn, text) in &snapshot.feedback {
            let index = self.ports.feedback_index(conn).ok_or_else(|| {
                mismatch(format!(
                    "`{conn}` is not a feedback connection of this graph"
                ))
            })?;
            let from = self.ports.feedback()[index].from;
            let port = &self.ports.outputs()[from].port;
            feedback[index] = Some(self.parse_port(port, PortDirection::Output, text)?);
        }
        let mut work: HashMap<usize, OwedWork<X::Value>> = HashMap::new();
        for island in &snapshot.islands {
            let mut members = island.members.clone();
            members.sort();
            let index =
                self.island_index.get(&members).copied().ok_or_else(|| {
                    mismatch(format!("no island with members {:?}", island.members))
                })?;
            let outgoing = &self.slots[index].feedback_out;
            let mut stale: PortSet<FeedbackIdx> = PortSet::new(self.feedback.len());
            for conn in &island.stale_feedback {
                let Some(index) = self
                    .ports
                    .feedback_index(conn)
                    .filter(|index| outgoing.contains(index))
                else {
                    return Err(mismatch(format!(
                        "`{conn}` is not a feedback connection out of island {:?}",
                        island.members
                    )));
                };
                stale.insert(index);
            }
            let replay = match &island.running {
                None if !island.stale_feedback.is_empty() => {
                    return Err(mismatch(format!(
                        "island {:?} lists stale feedback but no running generation",
                        island.members
                    )));
                }
                None => None,
                Some(by_member) => Some(Replay {
                    inputs: self.parse_replay(index, by_member)?,
                    stale,
                }),
            };
            if work.insert(index, (replay, island.queued)).is_some() {
                return Err(mismatch(format!(
                    "island {:?} is listed twice",
                    island.members
                )));
            }
        }

        // Nothing fails from here on.
        self.inputs = inputs;
        self.outputs = outputs;
        self.feedback = feedback;
        for index in 0..self.slots.len() {
            // Guest state must not outlive the restore: every island drops
            // its Store and is rebuilt when it next runs.
            if !matches!(
                self.slots[index].state.stop_cause(),
                Some(StopCause::Restored)
            ) {
                self.transition(index, |state| state.stop(StopCause::Restored));
            }
            let slot = &mut self.slots[index];
            let (replay, queued) = work.remove(&index).unwrap_or((None, false));
            slot.owed.clear();
            slot.stale_feedback.clear();
            if let Some(replay) = replay {
                slot.owed.set_replay(replay);
            }
            if queued {
                slot.owed.push_latched();
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// `val` as WAVE text; `at` says which value it is, if it cannot be.
    fn wave(&self, val: &X::Value, at: &dyn Fn() -> String) -> Result<String, SnapshotError> {
        self.executor
            .to_wave(val)
            .map_err(|message| SnapshotError { at: at(), message })
    }

    /// The latched values of one direction's ports as WAVE text, by node and
    /// port. A handle is left out: it is the host's, not text.
    fn latched_values<I: Idx>(
        &self,
        entries: &IndexVec<I, PortEntry>,
        latches: &Latches<I, X::Value>,
    ) -> Result<PortValues, SnapshotError> {
        let mut out = PortValues::new();
        for (index, val) in latches.iter_enumerated() {
            let entry = &entries[index];
            if let Some(val) = val
                && entry.kind != PortKind::Handle
            {
                let port = &entry.port;
                let text = self.wave(val, &|| format!("`{}.{}`", port.node, port.port))?;
                out.entry(port.node.clone())
                    .or_default()
                    .insert(port.port.clone(), text);
            }
        }
        Ok(out)
    }

    /// `text` parsed as a value of `port`.
    fn parse_port(
        &self,
        port: &PortRef,
        direction: PortDirection,
        text: &str,
    ) -> Result<Arc<X::Value>, RuntimeError> {
        let invalid = |message: String| RuntimeError::SnapshotValue {
            node: port.node.clone(),
            port: port.port.clone(),
            message,
        };
        let ty = self
            .executor
            .port_type(port, direction)
            .ok_or_else(|| invalid(format!("not a Value {direction} port")))?;
        self.executor
            .parse_wave(ty, text)
            .map(|val| Arc::new(val.into_inner()))
            .map_err(invalid)
    }

    /// One direction's values of a snapshot, parsed, by port.
    fn parse_values<I: Idx>(
        &self,
        values: &PortValues,
        direction: PortDirection,
        len: usize,
        index_of: impl Fn(&NodeId, &PortName) -> Option<I>,
    ) -> Result<Latches<I, X::Value>, RuntimeError> {
        let mut out = IndexVec::from_fn(len, |_| None);
        for (node, ports) in values {
            for (port, text) in ports {
                let reference = PortRef::new(node.clone(), port.clone());
                let val = self.parse_port(&reference, direction, text)?;
                if let Some(index) = index_of(node, port) {
                    out[index] = Some(val);
                }
            }
        }
        Ok(out)
    }

    /// Parses a replay's recorded inputs for island `index`: only external
    /// Value inputs, and every required one.
    fn parse_replay(
        &self,
        index: usize,
        by_member: &PortValues,
    ) -> Result<StartInputs<X::Value>, RuntimeError> {
        let plan = &self.slots[index].plan;
        let mut external: StartInputs<X::Value> = plan
            .members
            .iter()
            .map(|m| vec![None; m.field_count()])
            .collect();
        for (node, ports) in by_member {
            let position = plan
                .members
                .iter()
                .position(|m| &m.node == node)
                .ok_or_else(|| RuntimeError::SnapshotMismatch {
                    message: format!("`{node}` is not in its island"),
                })?;
            let member = &plan.members[position];
            for (port, text) in ports {
                let invalid = |message: &str| RuntimeError::SnapshotValue {
                    node: node.clone(),
                    port: port.clone(),
                    message: message.into(),
                };
                let (field, input) = member
                    .external_values()
                    .find(|(_, input)| &input.name == port)
                    .ok_or_else(|| {
                        invalid("not an external Value input of the replayed generation")
                    })?;
                let ty = self
                    .executor
                    .port_type(&input.port, PortDirection::Input)
                    .ok_or_else(|| invalid("not a Value input port"))?;
                let val = self
                    .executor
                    .parse_wave(ty, text)
                    .map_err(|e| invalid(&e))?;
                external[position][field] = Some(Arc::new(val.into_inner()));
            }
        }
        for (member, values) in plan.members.iter().zip(&external) {
            if let Some((_, input)) = member
                .external_values()
                .find(|(field, input)| !input.optional && values[*field].is_none())
            {
                return Err(RuntimeError::SnapshotValue {
                    node: member.node.clone(),
                    port: input.name.clone(),
                    message: "the replayed generation has no value for this required input".into(),
                });
            }
        }
        Ok(external)
    }
}
