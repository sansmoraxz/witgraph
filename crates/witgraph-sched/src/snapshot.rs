//! Snapshots of the host-visible state, and restoring them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use witgraph_ir::{
    FeedbackIdx, Idx, IndexVec, NodeId, PortDirection, PortEntry, PortKind, PortName, PortRef,
    PortSet,
};
pub use witgraph_traits::snapshot::*;

use crate::control::SchedulerCore;
use crate::error::RuntimeError;
use crate::executor::{DataPlane, Executor};
use crate::island::{Replay, StopCause};
use crate::latches::{Latches, StartInputs};
use crate::mode::RuntimeMode;
use crate::scheduler::Scheduler;

/// What a snapshot says an island owes: a replay, and whether a run on the
/// latched inputs is queued.
type OwedWork<V> = (Option<(StartInputs<V>, Replay)>, bool);

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
        let core = &self.core;
        let latches = core.plane.lock();
        // A handle is the host's, not text: it is left out, and the island
        // that produced it owes a run, which makes a new one.
        let producers: BTreeSet<usize> = latches
            .outputs
            .iter_enumerated()
            .filter(|(output, val)| {
                val.is_some() && core.ports.outputs()[*output].kind == PortKind::Handle
            })
            .map(|(output, _)| core.island_of_node[core.ports.outputs()[output].node])
            .collect();
        let mut feedback = BTreeMap::new();
        for (conn, val) in latches.feedback.iter_enumerated() {
            let entry = &core.ports.feedback()[conn];
            if let Some(val) = val
                && core.ports.outputs()[entry.from].kind != PortKind::Handle
            {
                let id = &entry.id;
                feedback.insert(id.clone(), wave(val, &|| format!("feedback `{id}`"))?);
            }
        }
        let mut islands = Vec::new();
        for (index, slot) in core.slots.iter().enumerate() {
            // In flight, or a restored replay not started yet: its inputs,
            // and what it must not feed back.
            let started = if slot.state.is_running() {
                latches.started[index]
                    .as_ref()
                    .map(|inputs| (inputs, &slot.stale_feedback))
            } else {
                slot.owed.replay().and_then(|replay| {
                    latches.replays[index]
                        .as_ref()
                        .map(|inputs| (inputs, &replay.stale))
                })
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
                        .map(|conn| core.ports.feedback()[conn].id.clone())
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
            nodes: core.components.clone(),
            connections: core.connections.clone(),
            links: core.links.clone(),
            quiescent: core.running == 0,
            phases: core
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
            inputs: self.latched_values(core.ports.inputs(), &latches.inputs)?,
            outputs: self.latched_values(core.ports.outputs(), &latches.outputs)?,
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
        self.core().drain_events();
        if self.core.running > 0 {
            return Err(RuntimeError::NotQuiescent);
        }
        let mismatch = |message: String| RuntimeError::SnapshotMismatch { message };
        let ours = &self.core.components;
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
        if snapshot.connections != self.core.connections {
            return Err(mismatch("the graph's connections differ".into()));
        }
        if snapshot.links != self.core.links {
            return Err(mismatch("the graph's links differ".into()));
        }
        let ports = self.core.ports.clone();
        let inputs = self.parse_values(
            &snapshot.inputs,
            PortDirection::Input,
            ports.inputs().len(),
            |node, port| ports.input(node, port),
        )?;
        let outputs = self.parse_values(
            &snapshot.outputs,
            PortDirection::Output,
            ports.outputs().len(),
            |node, port| ports.output(node, port),
        )?;
        let mut feedback = IndexVec::from_fn(ports.feedback().len(), |_| None);
        for (conn, text) in &snapshot.feedback {
            let index = ports.feedback_index(conn).ok_or_else(|| {
                mismatch(format!(
                    "`{conn}` is not a feedback connection of this graph"
                ))
            })?;
            let from = ports.feedback()[index].from;
            let port = &ports.outputs()[from].port;
            feedback[index] = Some(self.parse_port(port, PortDirection::Output, text)?);
        }
        let mut work: std::collections::HashMap<usize, OwedWork<X::Value>> =
            std::collections::HashMap::new();
        for island in &snapshot.islands {
            let mut members = island.members.clone();
            members.sort();
            let index = self
                .core
                .island_index
                .get(&members)
                .copied()
                .ok_or_else(|| mismatch(format!("no island with members {:?}", island.members)))?;
            let outgoing = &self.core.slots[index].feedback_out;
            let mut stale: PortSet<FeedbackIdx> = PortSet::new(ports.feedback().len());
            for conn in &island.stale_feedback {
                let Some(index) = ports
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
                Some(by_member) => Some((self.parse_replay(index, by_member)?, Replay { stale })),
            };
            if work.insert(index, (replay, island.queued)).is_some() {
                return Err(mismatch(format!(
                    "island {:?} is listed twice",
                    island.members
                )));
            }
        }

        // Nothing fails from here on.
        let islands = self.core.slots.len();
        let mut replays: Vec<Option<StartInputs<X::Value>>> = (0..islands).map(|_| None).collect();
        let mut owed: Vec<(Option<Replay>, bool)> = (0..islands).map(|_| (None, false)).collect();
        for (index, (replay, queued)) in work {
            let replay = replay.map(|(inputs, replay)| {
                replays[index] = Some(inputs);
                replay
            });
            owed[index] = (replay, queued);
        }
        {
            let mut latches = self.core.plane.lock();
            latches.inputs = inputs;
            latches.outputs = outputs;
            latches.feedback = feedback;
            latches.replays = replays;
        }
        let presence: Vec<_> = {
            let latches = self.core.plane.lock();
            latches
                .inputs
                .iter_enumerated()
                .filter(|(_, val)| val.is_some())
                .map(|(input, _)| input)
                .collect()
        };
        let pending: Vec<_> = {
            let latches = self.core.plane.lock();
            latches
                .feedback
                .iter_enumerated()
                .filter(|(_, val)| val.is_some())
                .map(|(conn, _)| conn)
                .collect()
        };
        self.core().apply_restore(&presence, &pending, owed);
        Ok(())
    }

    /// `val` as WAVE text; `at` says which value it is, if it cannot be.
    fn wave(&self, val: &X::Value, at: &dyn Fn() -> String) -> Result<String, SnapshotError> {
        self.core
            .plane
            .executor
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
        let executor = &self.core.plane.executor;
        let ty = executor
            .port_type(port, direction)
            .ok_or_else(|| invalid(format!("not a Value {direction} port")))?;
        executor
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
        let plan = &self.core.slots[index].plan;
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
                    .core
                    .plane
                    .executor
                    .port_type(&input.port, PortDirection::Input)
                    .ok_or_else(|| invalid("not a Value input port"))?;
                let val = self
                    .core
                    .plane
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

impl SchedulerCore<dyn DataPlane> {
    /// Puts a restored state in place: what `presence` and `pending` name
    /// holds a value, each island owes what `owed` says, and no guest state
    /// outlives the restore.
    fn apply_restore(
        &mut self,
        presence: &[witgraph_ir::InputIdx],
        pending: &[FeedbackIdx],
        owed: Vec<(Option<Replay>, bool)>,
    ) {
        self.presence.clear();
        for input in presence {
            self.presence.insert(*input);
        }
        self.pending.clear();
        for conn in pending {
            self.pending.insert(*conn);
        }
        self.versions.bump_all();
        for (index, (replay, queued)) in owed.into_iter().enumerate() {
            // Guest state must not outlive the restore: every island drops
            // its Store and is rebuilt when it next runs.
            if !matches!(
                self.slots[index].state.stop_cause(),
                Some(StopCause::Restored)
            ) {
                self.transition(index, |state| state.stop(StopCause::Restored));
            }
            let slot = &mut self.slots[index];
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
    }
}
