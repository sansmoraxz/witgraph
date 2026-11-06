//! Error types for the witgraph runtime.
//!
//! Two tiers: [`RuntimeError`] for failures the host sees when loading or
//! driving the graph, and [`NodeFault`] for a failure inside the graph,
//! which faults every node of it.

use witgraph_ir::{ConnectionId, NodeId, PortDirection, PortName};

/// A graph-level runtime error: a scheduler that cannot be built, or a host
/// call with bad arguments. What loading components can fail with besides
/// is the executor's (the wasmtime runtime's `LoadError`).
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum RuntimeError {
    /// A read or write was given up on ([`until`](crate::until)).
    #[error("given up on: a guest may be spinning")]
    #[diagnostic(code(witgraph::runtime::pull_stopped))]
    PullStopped,
    /// The instance stopped before a read or write was served.
    #[error("the graph stopped before it was served")]
    #[diagnostic(code(witgraph::runtime::instance_stopped))]
    InstanceStopped,
    /// A restore was attempted while a `run` is in flight. Restore
    /// onto a freshly loaded graph, or after `shutdown`/`cancel`.
    #[error("a `run` is in flight; restore needs a graph with nothing in flight")]
    #[diagnostic(code(witgraph::runtime::not_quiescent))]
    NotQuiescent,
    /// A snapshot was taken from a different graph.
    #[error("snapshot does not belong to this graph: {message}")]
    #[diagnostic(code(witgraph::runtime::snapshot_mismatch))]
    SnapshotMismatch {
        /// What differs.
        message: String,
    },
    /// A snapshot value is not valid for its port.
    #[error("snapshot value for `{node}.{port}` is not valid: {message}")]
    #[diagnostic(code(witgraph::runtime::snapshot_value))]
    SnapshotValue {
        /// The node the value belongs to.
        node: NodeId,
        /// The port the value belongs to.
        port: PortName,
        /// Why it was rejected (WAVE parse error, unknown port, ...).
        message: String,
    },
    /// A referenced node does not exist in the graph.
    #[error("unknown node `{node}`")]
    #[diagnostic(code(witgraph::runtime::unknown_node))]
    UnknownNode {
        /// The missing node.
        node: NodeId,
    },
    /// A referenced port is not a Value input (for [`inject`] and
    /// [`clear_input`]) or Value or handle output (for [`read_output`]) of
    /// the node.
    ///
    /// [`inject`]: crate::RunningGraph::inject
    /// [`clear_input`]: crate::RunningGraph::clear_input
    /// [`read_output`]: crate::RunningGraph::read_output
    #[error("`{node}.{port}` is not a Value {direction} port")]
    #[diagnostic(code(witgraph::runtime::not_a_value_port))]
    NotAValuePort {
        /// The node.
        node: NodeId,
        /// The port.
        port: PortName,
        /// Which kind of port was expected.
        direction: PortDirection,
    },
    /// [`inject`](crate::RunningGraph::inject) targeted an input that a
    /// non-feedback connection writes. An input has one writer: inject
    /// only into unconnected inputs and inputs fed by feedback connections.
    #[error(
        "`{node}.{port}` is written by connection `{connection}`; only unconnected and feedback-fed inputs take injected values"
    )]
    #[diagnostic(code(witgraph::runtime::connected_input))]
    ConnectedInput {
        /// The node.
        node: NodeId,
        /// The port.
        port: PortName,
        /// The connection writing the port.
        connection: ConnectionId,
    },
    /// An injected value does not have the port's type.
    #[error("value for `{node}.{port}` does not have the port's type: {message}")]
    #[diagnostic(code(witgraph::runtime::value_type))]
    ValueType {
        /// The node.
        node: NodeId,
        /// The port.
        port: PortName,
        /// Why the value was rejected.
        message: String,
    },
    /// The runtime configuration is invalid.
    #[error("invalid config: {message}")]
    #[diagnostic(code(witgraph::runtime::invalid_config))]
    InvalidConfig {
        /// What is wrong with the configuration.
        message: String,
    },
}

/// Why a graph faulted. Every node of the graph carries the same fault;
/// a trap poisons the whole Store.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum NodeFault {
    /// A component in the graph trapped (spawned tasks included).
    #[error("WASM trap: {message}")]
    #[diagnostic(code(witgraph::runtime::wasm_trap))]
    WasmTrap {
        /// The trap message.
        message: String,
    },
    /// Growing a linear memory or table would have taken the graph past
    /// `RuntimeConfig::max_memory`.
    #[error("the graph needs {requested} bytes of memory, over its limit of {limit}")]
    #[diagnostic(code(witgraph::runtime::memory_limit))]
    MemoryLimit {
        /// The bytes the graph would have held after the growth.
        requested: usize,
        /// The graph's limit.
        limit: usize,
    },
    /// One call copied more out of a guest than
    /// `RuntimeConfig::hostcall_fuel`
    /// allows.
    #[error("a call copied more data out of the guest than `hostcall_fuel` allows")]
    #[diagnostic(code(witgraph::runtime::hostcall_fuel))]
    HostcallFuelExhausted,
    /// The graph burned its whole fuel budget
    /// (`RuntimeConfig::fuel_per_run`)
    /// before its next `run` started.
    #[error("fuel exhausted")]
    #[diagnostic(code(witgraph::runtime::fuel_exhausted))]
    FuelExhausted,
    /// A node called `witgraph:runtime/host.fatal`.
    #[error("fatal: {message}")]
    #[diagnostic(code(witgraph::runtime::fatal))]
    Fatal {
        /// The message passed to `fatal`.
        message: String,
    },
    /// Rebuilding the stopped graph, before its next run, failed: it failed to
    /// instantiate, or making its Store's data failed.
    #[error("restart failed: {message}")]
    #[diagnostic(code(witgraph::runtime::restart))]
    Restart {
        /// Why the graph could not be rebuilt.
        message: String,
    },
}

impl NodeFault {
    /// What kind of fault it is, as both hosts name it to JavaScript: `trap`,
    /// `memory-limit`, `hostcall-fuel`, `fuel`, `fatal` or `restart`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WasmTrap { .. } => "trap",
            Self::MemoryLimit { .. } => "memory-limit",
            Self::HostcallFuelExhausted => "hostcall-fuel",
            Self::FuelExhausted => "fuel",
            Self::Fatal { .. } => "fatal",
            Self::Restart { .. } => "restart",
        }
    }
}
