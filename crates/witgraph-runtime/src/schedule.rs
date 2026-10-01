//! Tick results.

use witgraph_ir::NodeId;

use crate::error::NodeFault;

/// The outcome of one [`RuntimeGraph::tick`](crate::RuntimeGraph::tick).
#[derive(Debug, Clone)]
pub enum TickResult {
    /// Work happened: a generation started or finished, or a feedback edge
    /// latched a new value. Tick again to continue.
    Progress,
    /// Nothing ran and nothing is waiting: the graph is quiescent.
    Idle,
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
