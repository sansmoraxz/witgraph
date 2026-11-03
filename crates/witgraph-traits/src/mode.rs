//! Runtime mode trait and implementations.
//!
//! [`RuntimeMode`] is a zero-cost generic parameter on
//! the scheduler. In [`Perf`] mode all
//! callbacks are empty. In [`Trace`] mode every
//! callback records a [`TraceEvent`] behind a mutex for post-mortem
//! inspection.

use std::collections::VecDeque;
use std::sync::Mutex;

use witgraph_ir::NodeId;

use crate::error::NodeFault;
use crate::node::NodePhase;

/// A trace event recorded by [`Trace`] mode.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// A node's `run` was called.
    RunStarted {
        /// The node.
        node: NodeId,
    },
    /// A node's `run` returned; its Value outputs are latched.
    RunReturned {
        /// The node.
        node: NodeId,
    },
    /// A stream group's runs returned, and no guest work was left: it may run
    /// again.
    GroupDrained {
        /// The group's members.
        nodes: Vec<NodeId>,
    },
    /// A node changed phase.
    PhaseTransition {
        /// The node changing phase.
        node: NodeId,
        /// The phase before the transition.
        from: NodePhase,
        /// The phase after the transition.
        to: NodePhase,
    },
    /// A node faulted (every node of the graph is reported).
    Fault {
        /// The faulting node.
        node: NodeId,
        /// The fault.
        fault: NodeFault,
    },
    /// A node was cancelled.
    Cancelled {
        /// The cancelled node.
        node: NodeId,
    },
    /// The graph was rebuilt into a fresh Store: after a fault, a
    /// cancel, a shutdown or a restore.
    Restarted {
        /// The restarted node.
        node: NodeId,
    },
}

/// Callbacks fired at runtime instrumentation points.
///
/// Takes `&self`; implementations that record state use interior
/// mutability (e.g. `Mutex`).
pub trait RuntimeMode: Send + Sync + 'static {
    /// A node's `run` was called.
    fn on_run_started(&self, _node: &NodeId) {}
    /// A node's `run` returned.
    fn on_run_returned(&self, _node: &NodeId) {}
    /// A stream group's runs returned, and no guest work was left.
    fn on_group_drained(&self, _nodes: &[NodeId]) {}
    /// A node changed phase.
    fn on_phase_transition(&self, _node: &NodeId, _from: NodePhase, _to: NodePhase) {}
    /// A node faulted.
    fn on_node_fault(&self, _node: &NodeId, _fault: &NodeFault) {}
    /// A node was cancelled.
    fn on_cancelled(&self, _node: &NodeId) {}
    /// The graph was rebuilt, and this node with it.
    fn on_restarted(&self, _node: &NodeId) {}
}

/// Performance mode: every instrumentation callback is a no-op, so it
/// costs nothing.
pub struct Perf;

impl RuntimeMode for Perf {}

/// Trace mode: every instrumentation callback records a [`TraceEvent`].
pub struct Trace {
    trace: Mutex<VecDeque<TraceEvent>>,
    /// The most events kept; `None` keeps every one.
    limit: Option<usize>,
}

impl Trace {
    /// Creates a trace mode with an empty trace that keeps every event
    /// until it is taken.
    pub fn new() -> Self {
        Self {
            trace: Mutex::new(VecDeque::new()),
            limit: None,
        }
    }

    /// Creates a trace mode that keeps only the latest `limit` events (at
    /// least one), dropping the oldest, so a trace nobody takes stays
    /// bounded.
    pub fn bounded(limit: usize) -> Self {
        Self {
            trace: Mutex::new(VecDeque::new()),
            limit: Some(limit.max(1)),
        }
    }

    /// A snapshot of every recorded event. Empty if the lock is poisoned.
    pub fn trace(&self) -> Vec<TraceEvent> {
        self.trace
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.iter().cloned().collect())
    }

    /// Takes every recorded event, leaving the trace empty, so a
    /// long-running graph's trace does not grow without bound. Empty if the
    /// lock is poisoned.
    pub fn take_trace(&self) -> Vec<TraceEvent> {
        self.trace
            .lock()
            .map_or_else(|_| Vec::new(), |mut g| std::mem::take(&mut *g).into())
    }

    fn record(&self, event: TraceEvent) {
        if let Ok(mut trace) = self.trace.lock() {
            if self.limit.is_some_and(|limit| trace.len() >= limit) {
                trace.pop_front();
            }
            trace.push_back(event);
        }
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeMode for Trace {
    fn on_run_started(&self, node: &NodeId) {
        self.record(TraceEvent::RunStarted { node: node.clone() });
    }

    fn on_run_returned(&self, node: &NodeId) {
        self.record(TraceEvent::RunReturned { node: node.clone() });
    }

    fn on_group_drained(&self, nodes: &[NodeId]) {
        self.record(TraceEvent::GroupDrained {
            nodes: nodes.to_vec(),
        });
    }

    fn on_phase_transition(&self, node: &NodeId, from: NodePhase, to: NodePhase) {
        self.record(TraceEvent::PhaseTransition {
            node: node.clone(),
            from,
            to,
        });
    }

    fn on_node_fault(&self, node: &NodeId, fault: &NodeFault) {
        self.record(TraceEvent::Fault {
            node: node.clone(),
            fault: fault.clone(),
        });
    }

    fn on_cancelled(&self, node: &NodeId) {
        self.record(TraceEvent::Cancelled { node: node.clone() });
    }

    fn on_restarted(&self, node: &NodeId) {
        self.record(TraceEvent::Restarted { node: node.clone() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_trace_keeps_the_latest_events() {
        let trace = Trace::bounded(2);
        for node in ["a", "b", "c", "d", "e"] {
            trace.on_run_started(&node.into());
        }
        let nodes: Vec<NodeId> = trace
            .take_trace()
            .into_iter()
            .map(|event| match event {
                TraceEvent::RunStarted { node } => node,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(nodes, [NodeId::from("d"), NodeId::from("e")]);
        assert!(trace.take_trace().is_empty());
    }
}
