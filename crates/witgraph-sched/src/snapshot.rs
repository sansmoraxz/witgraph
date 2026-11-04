//! Snapshots of the host-visible state, and restoring them, with the values in
//! the host.

use std::collections::BTreeMap;
use std::sync::Arc;

use witgraph_ir::{
    FeedbackIdx, Idx, IndexVec, InputIdx, NodeId, PortDirection, PortEntry, PortKind, PortName,
    PortRef,
};
pub use witgraph_traits::snapshot::*;

use crate::control::SchedulerCore;
use crate::error::RuntimeError;
use crate::executor::{DataPlane, Executor};
use crate::latches::Latches;
use crate::mode::RuntimeMode;
use crate::scheduler::Scheduler;

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
    /// Captures the graph's host-visible state, at any time: latched Value
    /// inputs and outputs, pending feedback, every node's phase, and the runs in
    /// flight or owed. See [`Snapshot`] for what is and is not captured.
    ///
    /// Fails, naming it, on a value the executor cannot write as WAVE text.
    pub fn snapshot(&mut self) -> Result<Snapshot, SnapshotError> {
        self.core().serve_queued();
        let core: &SchedulerCore<dyn DataPlane> = &self.core;
        let mut snapshot = core.snapshot_base();
        let latches = self.core.plane.lock();
        let ports = core.ports();
        for (conn, val) in latches.feedback.iter_enumerated() {
            let entry = &ports.feedback()[conn];
            if let Some(val) = val
                && ports.outputs()[entry.from].kind != PortKind::Handle
            {
                let id = &entry.id;
                let text = self.wave(val, &|| format!("feedback `{id}`"))?;
                snapshot.feedback.insert(id.clone(), text);
            }
        }
        snapshot.inputs = self.latched_values(ports.inputs(), &latches.inputs)?;
        snapshot.outputs = self.latched_values(ports.outputs(), &latches.outputs)?;
        Ok(snapshot)
    }

    /// Replaces the graph's host-visible state with `snapshot`, on a new
    /// instance. The next [`tick`](Self::tick) re-runs what was in flight, on
    /// the latched inputs, and what was owed.
    ///
    /// The graph must have nothing in flight ([`RuntimeError::NotQuiescent`])
    /// and exactly the snapshot's nodes (every one with a phase), components (by
    /// content hash), connections and links ([`RuntimeError::SnapshotMismatch`]).
    /// Every value is parsed against its port's type
    /// ([`RuntimeError::SnapshotValue`]); on any error nothing is changed. Node
    /// phases and [`Snapshot::quiescent`] are informational and not restored,
    /// and no guest state outlives a restore.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.core().check_snapshot(snapshot)?;
        let ports = self.core.ports().clone();
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
            let index =
                ports
                    .feedback_index(conn)
                    .ok_or_else(|| RuntimeError::SnapshotMismatch {
                        message: format!("`{conn}` is not a feedback connection of this graph"),
                    })?;
            let from = ports.feedback()[index].from;
            let port = &ports.outputs()[from].port;
            feedback[index] = Some(self.parse_port(port, PortDirection::Output, text)?);
        }

        // Nothing fails from here on.
        self.core().restart_for_restore();
        let presence: Vec<InputIdx> = inputs
            .iter_enumerated()
            .filter(|(_, val)| val.is_some())
            .map(|(input, _)| input)
            .collect();
        let pending: Vec<FeedbackIdx> = feedback
            .iter_enumerated()
            .filter(|(_, val)| val.is_some())
            .map(|(conn, _)| conn)
            .collect();
        self.core
            .plane
            .lock()
            .restore(inputs, outputs, feedback, |input| {
                self.core.connected(input).is_none()
            });
        self.core().apply_restore(&presence, &pending, snapshot);
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
        let mut out: PortValues = BTreeMap::new();
        for (index, val) in latches.iter_enumerated() {
            let entry = &entries[index];
            if let Some(val) = val
                && entry.kind == PortKind::Value
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
}
