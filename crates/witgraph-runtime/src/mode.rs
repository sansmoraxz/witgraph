//! Runtime mode trait and implementations.
//!
//! [`RuntimeMode`] is a zero-cost generic parameter on
//! [`RuntimeGraph`](crate::graph::RuntimeGraph). In [`Perf`] mode all
//! callbacks are empty and monomorphized away. In [`Trace`] mode every
//! callback records a [`TraceEvent`] behind a mutex for post-mortem
//! inspection.
//!
//! Stream and future items move guest-to-guest and are never seen by the
//! host, so there are no per-item events.

use std::sync::Mutex;

use witgraph_ir::NodeId;

use crate::error::NodeFault;
use crate::node::NodePhase;

/// A trace event recorded by [`Trace`] mode.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// An island started a generation.
    GenerationStarted {
        /// The island's index in
        /// [`CompiledGraph::islands`](witgraph_ir::CompiledGraph::islands).
        island: usize,
        /// The island's generation counter, starting at 1.
        generation: u64,
    },
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
    /// Every `run` of an island's generation returned and no guest task is
    /// left in its Store.
    GenerationFinished {
        /// The island's index.
        island: usize,
        /// The generation that finished.
        generation: u64,
    },
    /// An island's generation ended without finishing: it faulted, or the
    /// host cancelled it or shut the graph down. Every `GenerationStarted`
    /// is paired with a `GenerationFinished` or a `GenerationStopped`.
    GenerationStopped {
        /// The island's index.
        island: usize,
        /// The generation that stopped.
        generation: u64,
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
    /// A node faulted (every node of the faulting island is reported).
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
    /// A node's island was rebuilt into a fresh Store: after a fault, a
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
    /// An island started a generation.
    fn on_generation_started(&self, _island: usize, _generation: u64) {}
    /// A node's `run` was called.
    fn on_run_started(&self, _node: &NodeId) {}
    /// A node's `run` returned.
    fn on_run_returned(&self, _node: &NodeId) {}
    /// An island's generation finished.
    fn on_generation_finished(&self, _island: usize, _generation: u64) {}
    /// An island's generation ended without finishing (a fault, a cancel,
    /// or shutdown).
    fn on_generation_stopped(&self, _island: usize, _generation: u64) {}
    /// A node changed phase.
    fn on_phase_transition(&self, _node: &NodeId, _from: NodePhase, _to: NodePhase) {}
    /// A node faulted.
    fn on_node_fault(&self, _node: &NodeId, _fault: &NodeFault) {}
    /// A node was cancelled.
    fn on_cancelled(&self, _node: &NodeId) {}
    /// A node's island was rebuilt.
    fn on_restarted(&self, _node: &NodeId) {}
}

/// Performance mode: every instrumentation callback is a no-op, so it
/// costs nothing.
pub struct Perf;

impl RuntimeMode for Perf {}

/// Trace mode: every instrumentation callback records a [`TraceEvent`].
pub struct Trace {
    trace: Mutex<Vec<TraceEvent>>,
}

impl Trace {
    /// Creates a trace mode with an empty trace.
    pub fn new() -> Self {
        Self {
            trace: Mutex::new(Vec::new()),
        }
    }

    /// A snapshot of every recorded event. Empty if the lock is poisoned.
    pub fn trace(&self) -> Vec<TraceEvent> {
        self.trace.lock().map_or_else(|_| Vec::new(), |g| g.clone())
    }

    /// Takes every recorded event, leaving the trace empty, so a
    /// long-running graph's trace does not grow without bound. Empty if the
    /// lock is poisoned.
    pub fn take_trace(&self) -> Vec<TraceEvent> {
        self.trace
            .lock()
            .map_or_else(|_| Vec::new(), |mut g| std::mem::take(&mut *g))
    }

    fn record(&self, event: TraceEvent) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(event);
        }
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeMode for Trace {
    fn on_generation_started(&self, island: usize, generation: u64) {
        self.record(TraceEvent::GenerationStarted { island, generation });
    }

    fn on_run_started(&self, node: &NodeId) {
        self.record(TraceEvent::RunStarted { node: node.clone() });
    }

    fn on_run_returned(&self, node: &NodeId) {
        self.record(TraceEvent::RunReturned { node: node.clone() });
    }

    fn on_generation_finished(&self, island: usize, generation: u64) {
        self.record(TraceEvent::GenerationFinished { island, generation });
    }

    fn on_generation_stopped(&self, island: usize, generation: u64) {
        self.record(TraceEvent::GenerationStopped { island, generation });
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
