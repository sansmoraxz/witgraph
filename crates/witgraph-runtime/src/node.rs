//! A node's lifecycle state, projected from its island.
//!
//! Every node of an island starts, finishes, faults and is cancelled
//! together, so a node has no state machine of its own: the island carries
//! the lifecycle (a typestate in the crate-private `island` module), and
//! [`NodeState`] is a read-only view of where the node's island is.
//!
//! | island phase               | node phase                         |
//! |----------------------------|------------------------------------|
//! | loaded or restored, not run | [`Pending`](NodePhase::Pending)   |
//! | generation in flight       | [`Running`](NodePhase::Running)     |
//! | generation finished        | [`Idle`](NodePhase::Idle)           |
//! | stopped by a fault         | [`Faulted`](NodePhase::Faulted)     |
//! | stopped by cancel/shutdown | [`Cancelled`](NodePhase::Cancelled) |
//!
//! `Running` covers the entire generation, including guest work that
//! continues after the node's own `run` has returned (a stream writer,
//! say). A stopped island is rebuilt at the start of its next generation,
//! so its nodes go straight to `Running`.

use witgraph_ir::{NodeId, NodeShape};

use crate::error::NodeFault;

/// Where a node is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodePhase {
    /// Its island has not run since the graph was loaded, or since a
    /// restore.
    Pending,
    /// Its island's generation is in flight.
    Running,
    /// Its island finished its last generation; it runs again when an
    /// input changes.
    Idle,
    /// Its island faulted. Restartable: new input, or
    /// [`rerun`](crate::RuntimeGraph::rerun), rebuilds the island.
    Faulted,
    /// Its island was cancelled or shut down. Restartable: new input, or
    /// [`rerun`](crate::RuntimeGraph::rerun), rebuilds the island.
    Cancelled,
}

impl NodePhase {
    /// Returns `true` for [`Faulted`](Self::Faulted) and
    /// [`Cancelled`](Self::Cancelled): the island was stopped and is rebuilt
    /// before it runs again. An island stopped by a restore has no Store
    /// either, but reads as [`Pending`](Self::Pending).
    pub fn is_stopped(self) -> bool {
        matches!(self, NodePhase::Faulted | NodePhase::Cancelled)
    }
}

/// A node's lifecycle state: a read-only view of its island's state at the
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
    pub(crate) fn new(
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

    /// Why the node's island faulted, if the node is
    /// [`Faulted`](NodePhase::Faulted). Every member of the island carries
    /// the same fault; see [`culprit`](Self::culprit) for which one caused
    /// it.
    pub fn fault_cause(&self) -> Option<&NodeFault> {
        self.fault.as_ref()
    }

    /// The member of the node's island that caused its fault, when that is
    /// known: the node that called `fatal`, the one that failed to
    /// instantiate on a rebuild, or the only member whose `run` had
    /// started and not returned.
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
