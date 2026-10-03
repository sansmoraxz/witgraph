//! Tick results and fault reports.

use witgraph_ir::NodeId;

use crate::error::NodeFault;

/// The outcome of one [`RuntimeGraph::tick`](crate::RuntimeGraph::tick).
#[derive(Debug, Clone)]
#[must_use = "a tick can end in an abort or a step limit"]
pub enum TickResult {
    /// Work happened: a generation started or finished (or faulted), or a
    /// feedback edge latched a new value. Tick again to continue. Faults
    /// other than `fatal` do not end a tick: see
    /// [`RuntimeGraph::take_faults`](crate::RuntimeGraph::take_faults).
    Progress,
    /// Nothing ran, and nothing can make progress: every island is idle,
    /// stopped, or owes a run it cannot start (a required input has no
    /// value yet).
    Idle,
    /// The `stop` future of
    /// [`RuntimeGraph::tick_until`](crate::RuntimeGraph::tick_until) fired
    /// first. The tick ended as a dropped one does: generations in flight
    /// carry on with the next tick, and every feedback loop whose iteration
    /// is over was latched.
    Interrupted,
    /// The tick started `max_steps_per_tick` generations before reaching
    /// quiescence. Generations still in flight keep their state and resume
    /// on the next tick.
    StepLimitReached,
    /// A node called `fatal`. The tick stopped at once; its island is
    /// faulted, and other in-flight generations resume on the next tick.
    Aborted {
        /// The node that called `fatal`.
        node: NodeId,
        /// The fatal fault.
        fault: NodeFault,
    },
}

/// An island fault, as [`RuntimeGraph::take_faults`](crate::RuntimeGraph::take_faults)
/// reports it.
#[derive(Debug, Clone)]
pub struct FaultReport {
    /// The island's members, in island order: every one of them faulted.
    pub members: Vec<NodeId>,
    /// The member that caused the fault, when it is known: the one that
    /// called `fatal` or failed to rebuild, or the island's only member.
    pub culprit: Option<NodeId>,
    /// The fault.
    pub fault: NodeFault,
}
