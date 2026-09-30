//! Node lifecycle state machine.
//!
//! Each node in a runtime graph has a [`NodePhase`] that tracks where it
//! is in its lifecycle. [`NodeState`] bundles the phase with the node's
//! identity and bookkeeping.

use witgraph_ir::{ConsumptionMode, NodeId};

/// The lifecycle phase of a node.
///
/// Transitions:
/// - `Created` -> `Draining` (if the node has drained inputs)
/// - `Created` -> `Ready` (if no drained inputs)
/// - `Draining` -> `Ready` (all drained inputs completed)
/// - `Ready` -> `Running` (scheduler activates the node)
/// - `Running` -> `Ready` (activation returns `Continue`)
/// - `Running` -> `Suspended` (async node waiting for input)
/// - `Running` -> `Completed` (activation returns `Completed`)
/// - `Running` -> `Faulted` (trap or error during activation)
/// - `Suspended` -> `Running` (new input arrives)
/// - `Completed` and `Faulted` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodePhase {
    /// Allocated but not yet initialized.
    Created,
    /// Drained inputs are being consumed to completion.
    Draining,
    /// Waiting for an activation trigger.
    Ready,
    /// Currently inside `activate()`.
    Running,
    /// Async node waiting for a stream, event, or future.
    Suspended,
    /// Finished successfully.
    Completed,
    /// Unrecoverable error.
    Faulted,
    /// Cancelled by the host.
    Cancelled,
}

impl NodePhase {
    /// Returns `true` if this phase is a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, NodePhase::Completed | NodePhase::Faulted | NodePhase::Cancelled)
    }

    /// Returns `true` if the given transition is valid per the lifecycle
    /// state machine.
    pub fn can_transition_to(self, target: NodePhase) -> bool {
        matches!(
            (self, target),
            (NodePhase::Created, NodePhase::Draining)
                | (NodePhase::Created, NodePhase::Ready)
                | (NodePhase::Created, NodePhase::Faulted)
                | (NodePhase::Draining, NodePhase::Running)
                | (NodePhase::Draining, NodePhase::Ready)
                | (NodePhase::Draining, NodePhase::Faulted)
                | (NodePhase::Ready, NodePhase::Running)
                | (NodePhase::Ready, NodePhase::Faulted)
                | (NodePhase::Running, NodePhase::Ready)
                | (NodePhase::Running, NodePhase::Suspended)
                | (NodePhase::Running, NodePhase::Completed)
                | (NodePhase::Running, NodePhase::Faulted)
                | (NodePhase::Suspended, NodePhase::Running)
                | (NodePhase::Suspended, NodePhase::Faulted)
                | (NodePhase::Created, NodePhase::Cancelled)
                | (NodePhase::Draining, NodePhase::Cancelled)
                | (NodePhase::Ready, NodePhase::Cancelled)
                | (NodePhase::Running, NodePhase::Cancelled)
                | (NodePhase::Suspended, NodePhase::Cancelled)
        )
    }
}

/// The runtime state of a single node.
#[derive(Debug, Clone)]
pub struct NodeState {
    /// The node's identity.
    pub id: NodeId,
    /// The node's current lifecycle phase.
    pub phase: NodePhase,
    /// How the node consumes its inputs.
    pub mode: ConsumptionMode,
    /// The number of drained inputs still awaiting completion.
    pub pending_drains: usize,
    /// Whether the node's `init()` export has been called.
    pub initialized: bool,
}

impl NodeState {
    /// Creates a new node state with the given identity and consumption
    /// mode.
    ///
    /// Starts in [`NodePhase::Created`] with no pending drains and no
    /// version history.
    pub fn new(id: NodeId, mode: ConsumptionMode) -> Self {
        Self {
            id,
            phase: NodePhase::Created,
            mode,
            pending_drains: 0,
            initialized: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_transitions() {
        assert!(NodePhase::Created.can_transition_to(NodePhase::Draining));
        assert!(NodePhase::Created.can_transition_to(NodePhase::Ready));
        assert!(NodePhase::Created.can_transition_to(NodePhase::Faulted));
        assert!(!NodePhase::Created.can_transition_to(NodePhase::Running));
        assert!(!NodePhase::Created.can_transition_to(NodePhase::Completed));
    }

    #[test]
    fn draining_transitions() {
        assert!(NodePhase::Draining.can_transition_to(NodePhase::Running));
        assert!(NodePhase::Draining.can_transition_to(NodePhase::Ready));
        assert!(NodePhase::Draining.can_transition_to(NodePhase::Faulted));
        assert!(!NodePhase::Draining.can_transition_to(NodePhase::Completed));
    }

    #[test]
    fn ready_transitions() {
        assert!(NodePhase::Ready.can_transition_to(NodePhase::Running));
        assert!(NodePhase::Ready.can_transition_to(NodePhase::Faulted));
        assert!(!NodePhase::Ready.can_transition_to(NodePhase::Completed));
        assert!(!NodePhase::Ready.can_transition_to(NodePhase::Ready));
    }

    #[test]
    fn running_transitions() {
        assert!(NodePhase::Running.can_transition_to(NodePhase::Ready));
        assert!(NodePhase::Running.can_transition_to(NodePhase::Suspended));
        assert!(NodePhase::Running.can_transition_to(NodePhase::Completed));
        assert!(NodePhase::Running.can_transition_to(NodePhase::Faulted));
        assert!(!NodePhase::Running.can_transition_to(NodePhase::Created));
        assert!(!NodePhase::Running.can_transition_to(NodePhase::Draining));
    }

    #[test]
    fn suspended_transitions() {
        assert!(NodePhase::Suspended.can_transition_to(NodePhase::Running));
        assert!(NodePhase::Suspended.can_transition_to(NodePhase::Faulted));
        assert!(!NodePhase::Suspended.can_transition_to(NodePhase::Ready));
        assert!(!NodePhase::Suspended.can_transition_to(NodePhase::Completed));
    }

    #[test]
    fn cancelled_transitions() {
        for source in [
            NodePhase::Created,
            NodePhase::Draining,
            NodePhase::Ready,
            NodePhase::Running,
            NodePhase::Suspended,
        ] {
            assert!(source.can_transition_to(NodePhase::Cancelled));
        }
    }

    #[test]
    fn terminal_states() {
        assert!(NodePhase::Completed.is_terminal());
        assert!(NodePhase::Faulted.is_terminal());
        assert!(NodePhase::Cancelled.is_terminal());
        assert!(!NodePhase::Ready.is_terminal());
        assert!(!NodePhase::Running.is_terminal());

        for target in [
            NodePhase::Created,
            NodePhase::Draining,
            NodePhase::Ready,
            NodePhase::Running,
            NodePhase::Suspended,
            NodePhase::Completed,
            NodePhase::Faulted,
            NodePhase::Cancelled,
        ] {
            assert!(!NodePhase::Completed.can_transition_to(target));
            assert!(!NodePhase::Faulted.can_transition_to(target));
            assert!(!NodePhase::Cancelled.can_transition_to(target));
        }
    }

    #[test]
    fn new_node_state() {
        let state = NodeState::new("sensor".into(), ConsumptionMode::Sync);
        assert_eq!(state.phase, NodePhase::Created);
        assert_eq!(state.pending_drains, 0);
        assert!(!state.initialized);
    }
}
