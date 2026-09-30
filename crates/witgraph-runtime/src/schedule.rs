//! Scheduler events and tick results.
//!
//! [`SchedulerEvent`] represents an event that may trigger a node
//! activation. [`TickResult`] reports the outcome of one scheduler
//! round.

use witgraph_ir::{NodeId, PortName, PortRef};

use crate::error::NodeFault;
use witgraph_ir::Val;

/// An event that the scheduler routes to target channels and uses to
/// determine which nodes are activatable.
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    /// A latched value changed on a port.
    ValueChanged {
        /// The target input port.
        target: PortRef,
    },
    /// An event was enqueued on a port.
    EventEnqueued {
        /// The target input port.
        target: PortRef,
    },
    /// A stream item arrived on a port.
    StreamItem {
        /// The target input port.
        target: PortRef,
    },
    /// A stream closed on a port.
    StreamClosed {
        /// The target input port.
        target: PortRef,
    },
    /// A future resolved on a port.
    FutureResolved {
        /// The target input port.
        target: PortRef,
    },
    /// An externally injected input value.
    ExternalInput {
        /// The target node.
        node: NodeId,
        /// The target input port.
        port: PortName,
        /// The injected value.
        value: Val,
    },
}

/// The outcome of one scheduler tick.
#[derive(Debug, Clone)]
pub enum TickResult {
    /// At least one node was activated and produced output or consumed
    /// input.
    Progress,
    /// No nodes were activatable; the graph is quiescent.
    Idle,
    /// All nodes have completed.
    Completed,
    /// The per-tick step limit was reached before the graph became
    /// quiescent.
    StepLimitReached,
    /// A node signaled a fatal condition. The tick halted immediately.
    Aborted {
        /// The node that signaled fatal.
        node: NodeId,
        /// The fatal fault.
        fault: NodeFault,
    },
}
