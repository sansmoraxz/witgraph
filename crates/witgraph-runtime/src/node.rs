//! A node's lifecycle state, projected from its island.
//!
//! Every node of an island starts, finishes, faults and is cancelled
//! together, so a node has no state machine of its own: the island carries
//! the lifecycle (a typestate in the crate-private `island` module), and
//! [`NodeState`] is a read-only view of where the node's island is.
//!
//! | island phase               | node phase                         |
//! |----------------------------|------------------------------------|
//! | built, never ran           | [`Pending`](NodePhase::Pending)     |
//! | generation in flight       | [`Running`](NodePhase::Running)     |
//! | generation finished        | [`Idle`](NodePhase::Idle)           |
//! | stopped by a fault         | [`Faulted`](NodePhase::Faulted)     |
//! | stopped by cancel/shutdown | [`Cancelled`](NodePhase::Cancelled) |
//!
//! `Running` covers the entire generation, including guest work that
//! continues after the node's own `run` has returned (a stream writer,
//! say). A stopped island is rebuilt before it runs again, so its nodes go
//! back to `Pending`.

use witgraph_ir::{NodeId, NodeShape};

use crate::error::NodeFault;

/// Where a node is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodePhase {
    /// Its island has not run since it was built (or rebuilt).
    Pending,
    /// Its island's generation is in flight.
    Running,
    /// Its island finished its last generation; it runs again when an
    /// input changes.
    Idle,
    /// Its island faulted. Restartable: new input rebuilds the island.
    Faulted,
    /// Its island was cancelled or shut down. Restartable: new input
    /// rebuilds the island.
    Cancelled,
}

impl NodePhase {
    /// Returns `true` for [`Faulted`](Self::Faulted) and
    /// [`Cancelled`](Self::Cancelled): the island has no Store until it is
    /// rebuilt.
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
}

impl NodeState {
    pub(crate) fn new(
        id: NodeId,
        shape: NodeShape,
        phase: NodePhase,
        fault: Option<NodeFault>,
    ) -> Self {
        Self {
            id,
            shape,
            phase,
            fault,
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
    /// [`Faulted`](NodePhase::Faulted).
    pub fn fault_cause(&self) -> Option<&NodeFault> {
        self.fault.as_ref()
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
        );
        assert_eq!(state.id(), &NodeId::from("n"));
        assert_eq!(state.phase(), NodePhase::Faulted);
        assert!(matches!(
            state.fault_cause(),
            Some(NodeFault::FuelExhausted)
        ));
    }
}
