//! A node's lifecycle state.
//!
//! | node                            | phase                               |
//! |---------------------------------|-------------------------------------|
//! | not run since the graph was built | [`Pending`](NodePhase::Pending)   |
//! | its `run` is in flight          | [`Running`](NodePhase::Running)     |
//! | its `run` returned              | [`Idle`](NodePhase::Idle)           |
//! | the graph was stopped by a fault | [`Faulted`](NodePhase::Faulted)    |
//! | the graph was shut down         | [`Cancelled`](NodePhase::Cancelled) |
//!
//! A fault stops the whole graph, so every node faults together. A stopped
//! graph is rebuilt before its next run, and its nodes start over as `Pending`.

use witgraph_ir::{NodeId, NodeShape};

use crate::error::NodeFault;

/// Where a node is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodePhase {
    /// It has not run since the graph was loaded, or since a
    /// restore.
    Pending,
    /// Its `run` is in flight.
    Running,
    /// Its `run` returned; it runs again when an
    /// input changes.
    Idle,
    /// The graph faulted. Restartable: new input, or
    /// `rerun`, rebuilds the graph.
    Faulted,
    /// The graph was shut down. Restartable: new input, or
    /// `rerun`, rebuilds the graph.
    Cancelled,
}

impl NodePhase {
    /// Returns `true` for [`Faulted`](Self::Faulted) and
    /// [`Cancelled`](Self::Cancelled): the graph was stopped and is rebuilt
    /// before it runs again. A restored graph has a new Store
    /// either, but reads as [`Pending`](Self::Pending).
    pub fn is_stopped(self) -> bool {
        matches!(self, NodePhase::Faulted | NodePhase::Cancelled)
    }
}

/// A node's lifecycle state: a read-only view of the scheduler's state at the
/// moment it was taken.
#[derive(Debug, Clone)]
pub struct NodeState {
    id: NodeId,
    shape: NodeShape,
    phase: NodePhase,
    fault: Option<NodeFault>,
    culprit: Option<NodeId>,
}

impl NodeState {
    /// A node's state as its scheduler knows it.
    pub fn new(
        id: NodeId,
        shape: NodeShape,
        phase: NodePhase,
        fault: Option<NodeFault>,
        culprit: Option<NodeId>,
    ) -> Self {
        Self {
            id,
            shape,
            phase,
            fault,
            culprit,
        }
    }

    /// The node's identity.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// How the node takes part in execution, derived from its ports.
    pub fn shape(&self) -> NodeShape {
        self.shape
    }

    /// The phase the node is in.
    pub fn phase(&self) -> NodePhase {
        self.phase
    }

    /// Why the graph faulted, if the node is [`Faulted`](NodePhase::Faulted).
    /// Every node carries the same fault; see [`culprit`](Self::culprit) for
    /// which one caused it.
    pub fn fault_cause(&self) -> Option<&NodeFault> {
        self.fault.as_ref()
    }

    /// The node that caused the fault, when that is known: the one that called
    /// `fatal`, or the only one with a `run` in flight, with no stream group at
    /// work. Otherwise none: a node that returned may still be running a task it
    /// spawned.
    pub fn culprit(&self) -> Option<&NodeId> {
        self.culprit.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_phases() {
        assert!(NodePhase::Faulted.is_stopped());
        assert!(NodePhase::Cancelled.is_stopped());
        for phase in [NodePhase::Pending, NodePhase::Running, NodePhase::Idle] {
            assert!(!phase.is_stopped());
        }
    }

    #[test]
    fn a_view_carries_its_fault() {
        let state = NodeState::new(
            "n".into(),
            NodeShape::Reactive,
            NodePhase::Faulted,
            Some(NodeFault::FuelExhausted),
            Some("n".into()),
        );
        assert_eq!(state.id(), &NodeId::from("n"));
        assert_eq!(state.phase(), NodePhase::Faulted);
        assert!(matches!(
            state.fault_cause(),
            Some(NodeFault::FuelExhausted)
        ));
    }
}
