//! Runtime mode trait and implementations.
//!
//! [`RuntimeMode`] is a zero-cost generic parameter on
//! [`RuntimeGraph`](crate::graph::RuntimeGraph). In [`Release`] mode
//! all callbacks are empty and monomorphized away. In [`struct@Debug`] mode
//! every callback records a [`TraceEvent`] behind a mutex for
//! post-mortem inspection.

use std::sync::Mutex;

use witgraph_ir::{NodeId, PortRef};

use crate::abi::{Activation, ActivationResult};
use crate::error::NodeFault;
use crate::node::NodePhase;
use witgraph_ir::Val;

/// A trace event recorded by [`struct@Debug`] mode.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// Recorded before a node activation.
    BeforeActivate {
        /// The node being activated.
        node: NodeId,
        /// The activation reason.
        activation: Activation,
    },
    /// Recorded after a node activation.
    AfterActivate {
        /// The node that was activated.
        node: NodeId,
        /// The activation result.
        result: ActivationResult,
    },
    /// Recorded when a value is written to a channel.
    ChannelWrite {
        /// The source port.
        from: PortRef,
        /// The destination port.
        to: PortRef,
        /// The value written.
        val: Val,
    },
    /// Recorded when a node changes phase.
    PhaseTransition {
        /// The node changing phase.
        node: NodeId,
        /// The phase before the transition.
        from: NodePhase,
        /// The phase after the transition.
        to: NodePhase,
    },
    /// Recorded when a node faults.
    Fault {
        /// The faulting node.
        node: NodeId,
        /// The fault.
        fault: NodeFault,
    },
    /// Recorded when a node is cancelled.
    Cancelled {
        /// The cancelled node.
        node: NodeId,
    },
    /// Recorded when a cancelled node is re-instantiated.
    Restarted {
        /// The restarted node.
        node: NodeId,
    },
}

/// Callbacks fired at runtime instrumentation points.
///
/// Takes `&self` so callbacks can be called from concurrent actor
/// activations without borrow conflicts. Implementations that record
/// state use interior mutability (e.g. `Mutex`).
pub trait RuntimeMode: Send + Sync + 'static {
    /// Called before a node is activated.
    fn on_before_activate(&self, _node: &NodeId, _activation: &Activation) {}
    /// Called after a node activation completes.
    fn on_after_activate(&self, _node: &NodeId, _result: &ActivationResult) {}
    /// Called when a value is written through a channel.
    fn on_channel_write(&self, _from: &PortRef, _to: &PortRef, _val: &Val) {}
    /// Called when a node transitions between phases.
    fn on_phase_transition(&self, _node: &NodeId, _from: NodePhase, _to: NodePhase) {}
    /// Called when a node faults.
    fn on_node_fault(&self, _node: &NodeId, _fault: &NodeFault) {}
    /// Called when a node is cancelled.
    fn on_cancelled(&self, _node: &NodeId) {}
    /// Called when a cancelled node is re-instantiated.
    fn on_restarted(&self, _node: &NodeId) {}
}

/// Release mode: all instrumentation callbacks are no-ops,
/// monomorphized away by the compiler.
pub struct Release;

impl RuntimeMode for Release {}

/// Debug mode: every instrumentation callback records a [`TraceEvent`]
/// behind a mutex for post-mortem inspection.
pub struct Debug {
    trace: Mutex<Vec<TraceEvent>>,
}

impl Debug {
    /// Creates a new debug mode instance with an empty trace.
    pub fn new() -> Self {
        Self {
            trace: Mutex::new(Vec::new()),
        }
    }

    /// Returns a snapshot of all recorded trace events.
    ///
    /// Returns an empty vec if the lock is poisoned.
    pub fn trace(&self) -> Vec<TraceEvent> {
        self.trace.lock().map_or_else(|_| Vec::new(), |g| g.clone())
    }
}

impl Default for Debug {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeMode for Debug {
    fn on_before_activate(&self, node: &NodeId, activation: &Activation) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::BeforeActivate {
                node: node.clone(),
                activation: activation.clone(),
            });
        }
    }

    fn on_after_activate(&self, node: &NodeId, result: &ActivationResult) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::AfterActivate {
                node: node.clone(),
                result: *result,
            });
        }
    }

    fn on_channel_write(&self, from: &PortRef, to: &PortRef, val: &Val) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::ChannelWrite {
                from: from.clone(),
                to: to.clone(),
                val: val.clone(),
            });
        }
    }

    fn on_phase_transition(&self, node: &NodeId, from: NodePhase, to: NodePhase) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::PhaseTransition {
                node: node.clone(),
                from,
                to,
            });
        }
    }

    fn on_node_fault(&self, node: &NodeId, fault: &NodeFault) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::Fault {
                node: node.clone(),
                fault: fault.clone(),
            });
        }
    }

    fn on_cancelled(&self, node: &NodeId) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::Cancelled {
                node: node.clone(),
            });
        }
    }

    fn on_restarted(&self, node: &NodeId) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(TraceEvent::Restarted {
                node: node.clone(),
            });
        }
    }
}
