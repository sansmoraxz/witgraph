//! What the read methods of a `WebGraph` report while a tick holds its
//! scheduler: a breakpoint in `beforeRun` pauses the tick, and a debugger
//! still wants to look at the graph.
//!
//! The scheduler is the tick's until it ends, so the view is kept beside
//! it: copied from the scheduler at load and on a restore, then kept up to
//! date by the scheduler's [`RuntimeMode`] callbacks (phases, faults, the
//! trace) and the executor's calls (Value outputs). What those touched is
//! made exact again from the scheduler when a tick ends, so no tick copies
//! the whole graph.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use witgraph_ir::{NodeId, PortName};
use witgraph_sched::{NodeFault, NodePhase, RuntimeMode, Trace, TraceEvent};

/// A node's state as the view has it.
#[derive(Debug, Clone)]
pub(crate) struct LiveState {
    pub(crate) phase: NodePhase,
    pub(crate) fault: Option<NodeFault>,
    /// As of the last tick's end: a fault during a tick names none until
    /// the tick ends.
    pub(crate) culprit: Option<NodeId>,
}

/// The trace, and every node's state as of the last change.
pub(crate) struct Live {
    trace: Trace,
    states: Mutex<HashMap<NodeId, LiveState>>,
    /// The nodes whose state changed since the view was last made exact.
    touched: Mutex<HashSet<NodeId>>,
}

impl Live {
    /// A view whose trace keeps the latest `trace_limit` events.
    pub(crate) fn new(trace_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            // The JavaScript executor moves no stream items itself (the
            // loader taps streams in JavaScript): no `StreamItems` events.
            trace: Trace::bounded(trace_limit),
            states: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashSet::new()),
        })
    }

    /// Takes the trace recorded since the last call.
    pub(crate) fn take_trace(&self) -> Vec<TraceEvent> {
        self.trace.take_trace()
    }

    /// Sets a node's state, from the scheduler.
    pub(crate) fn set_state(&self, node: NodeId, state: LiveState) {
        if let Ok(mut states) = self.states.lock() {
            states.insert(node, state);
        }
    }

    /// Takes the nodes whose state changed since the last call.
    pub(crate) fn take_touched(&self) -> Vec<NodeId> {
        self.touched
            .lock()
            .map(|mut touched| touched.drain().collect())
            .unwrap_or_default()
    }

    /// A node's state, as of the last change.
    pub(crate) fn state(&self, node: &NodeId) -> Option<LiveState> {
        self.states.lock().ok()?.get(node).cloned()
    }

    fn update(&self, node: &NodeId, change: impl FnOnce(&mut LiveState)) {
        if let Ok(mut states) = self.states.lock()
            && let Some(state) = states.get_mut(node)
        {
            change(state);
        }
        if let Ok(mut touched) = self.touched.lock() {
            touched.insert(node.clone());
        }
    }
}

/// The scheduler's mode: records the trace and keeps the view's states.
pub(crate) struct LiveMode(pub(crate) Arc<Live>);

impl RuntimeMode for LiveMode {
    fn on_generation_started(&self, island: usize, generation: u64) {
        self.0.trace.on_generation_started(island, generation);
    }

    fn on_run_started(&self, node: &NodeId) {
        self.0.trace.on_run_started(node);
    }

    fn on_run_returned(&self, node: &NodeId) {
        self.0.trace.on_run_returned(node);
    }

    fn on_generation_finished(&self, island: usize, generation: u64) {
        self.0.trace.on_generation_finished(island, generation);
    }

    fn on_generation_stopped(&self, island: usize, generation: u64) {
        self.0.trace.on_generation_stopped(island, generation);
    }

    fn on_phase_transition(&self, node: &NodeId, from: NodePhase, to: NodePhase) {
        self.0.trace.on_phase_transition(node, from, to);
        self.0.update(node, |state| {
            state.phase = to;
            if to != NodePhase::Faulted {
                state.fault = None;
                state.culprit = None;
            }
        });
    }

    fn on_node_fault(&self, node: &NodeId, fault: &NodeFault) {
        self.0.trace.on_node_fault(node, fault);
        self.0.update(node, |state| {
            state.fault = Some(fault.clone());
            state.culprit = None;
        });
    }

    fn on_cancelled(&self, node: &NodeId) {
        self.0.trace.on_cancelled(node);
    }

    fn on_restarted(&self, node: &NodeId) {
        self.0.trace.on_restarted(node);
    }

    fn wants_stream_items(&self) -> bool {
        self.0.trace.wants_stream_items()
    }

    fn on_stream_items(&self, node: &NodeId, port: &PortName, count: usize) {
        self.0.trace.on_stream_items(node, port, count);
    }
}
