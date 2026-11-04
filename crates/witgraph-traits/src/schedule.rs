//! Tick results and fault reports.

use witgraph_ir::NodeId;

use crate::error::NodeFault;

/// The outcome of one `tick`.
#[derive(Debug, Clone)]
#[must_use = "a tick can end in a fault or a step limit"]
pub enum TickResult {
    /// Work happened: a node ran, or a feedback edge latched a new value. Tick
    /// again to continue.
    Progress,
    /// Nothing ran, and nothing can make progress: the graph is stopped, or
    /// every node is idle or owes a run it cannot start (a required input has
    /// no value yet).
    Idle,
    /// The `stop` future of
    /// `tick_until` fired
    /// first. The tick ended as a dropped one does: what is in flight carries
    /// on with the next tick, and every feedback loop whose iteration
    /// is over was latched.
    Interrupted,
    /// The tick started `max_steps_per_tick` runs before reaching quiescence.
    /// What is in flight resumes on the next tick.
    StepLimitReached,
    /// The graph faulted, which stopped all of it, and the tick with it. What the
    /// host asked for since runs on a new instance, from the next tick.
    /// `take_faults` reports the same fault, for as long as nobody has taken it.
    Faulted(FaultReport),
}

/// A fault, as `take_faults` reports it: it stopped the whole graph.
#[derive(Debug, Clone)]
pub struct FaultReport {
    /// The node that caused the fault, when it is known: the one that called
    /// `fatal`, or the only one with a `run` in flight when it happened, with no
    /// stream group at work (see [`NodeState::culprit`](crate::NodeState::culprit)).
    pub culprit: Option<NodeId>,
    /// The fault.
    pub fault: NodeFault,
}
