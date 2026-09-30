//! Error types for the witgraph runtime.
//!
//! Three tiers: [`RuntimeError`] for graph-level failures during loading,
//! [`NodeFault`] for per-node failures during execution, and
//! [`ChannelError`] for channel-level protocol violations.

use witgraph_ir::{ComponentRef, NodeId, PortName};

/// A graph-level runtime error, typically during loading or configuration.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum RuntimeError {
    /// A node's component has no corresponding WASM module.
    #[error("no WASM module provided for component `{component}`")]
    #[diagnostic(code(witgraph::runtime::missing_wasm))]
    MissingWasm {
        /// The component missing its WASM bytes.
        component: Box<ComponentRef>,
    },
    /// A referenced node does not exist in the graph.
    #[error("unknown node `{node}`")]
    #[diagnostic(code(witgraph::runtime::unknown_node))]
    UnknownNode {
        /// The missing node.
        node: NodeId,
    },
    /// A WASM component failed to instantiate.
    #[error("failed to instantiate node `{node}`: {message}")]
    #[diagnostic(code(witgraph::runtime::instantiation))]
    Instantiation {
        /// The node whose component could not be instantiated.
        node: NodeId,
        /// The underlying error message.
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

/// A per-node execution fault. The faulted node transitions to
/// [`Faulted`](crate::node::NodePhase::Faulted) and the graph continues
/// with reduced functionality, unless the fault is [`Fatal`](Self::Fatal).
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum NodeFault {
    /// The WASM component trapped.
    #[error("WASM trap: {message}")]
    #[diagnostic(code(witgraph::runtime::wasm_trap))]
    WasmTrap {
        /// The trap message.
        message: String,
    },
    /// The node exhausted its fuel budget.
    #[error("fuel exhausted")]
    #[diagnostic(code(witgraph::runtime::fuel_exhausted))]
    FuelExhausted,
    /// The node exceeded its epoch deadline.
    #[error("epoch interrupted")]
    #[diagnostic(code(witgraph::runtime::epoch_interrupted))]
    EpochInterrupted,
    /// An output channel overflowed its capacity.
    #[error("channel overflow on port `{port}` (capacity {capacity})")]
    #[diagnostic(code(witgraph::runtime::channel_overflow))]
    ChannelOverflow {
        /// The port whose channel overflowed.
        port: PortName,
        /// The channel's capacity limit.
        capacity: usize,
    },
    /// A future output was resolved more than once.
    #[error("future on port `{port}` already resolved")]
    #[diagnostic(code(witgraph::runtime::double_resolve))]
    DoubleResolve {
        /// The port whose future was double-resolved.
        port: PortName,
    },
    /// A write was attempted on a closed stream.
    #[error("write after close on port `{port}`")]
    #[diagnostic(code(witgraph::runtime::write_after_close))]
    WriteAfterClose {
        /// The port whose stream was already closed.
        port: PortName,
    },
    /// A value written to a port did not match the port's declared type.
    #[error("type mismatch on port `{port}`: expected {expected}, got {actual}")]
    #[diagnostic(code(witgraph::runtime::type_mismatch))]
    TypeMismatch {
        /// The port with the mismatched type.
        port: PortName,
        /// The expected type (rendered).
        expected: String,
        /// The actual type (rendered).
        actual: String,
    },
    /// The component signaled a graph-fatal condition via the `fatal()`
    /// host function. The tick halts immediately.
    #[error("fatal: {message}")]
    #[diagnostic(code(witgraph::runtime::fatal))]
    Fatal {
        /// The fatal error message from the component.
        message: String,
    },
}

/// A channel-level protocol violation.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum ChannelError {
    /// The channel is at capacity.
    #[error("channel is at capacity ({0})")]
    #[diagnostic(code(witgraph::runtime::channel_full))]
    Full(usize),
    /// A future slot was resolved more than once.
    #[error("future already resolved")]
    #[diagnostic(code(witgraph::runtime::already_resolved))]
    AlreadyResolved,
    /// A write was attempted on a closed stream.
    #[error("stream is closed")]
    #[diagnostic(code(witgraph::runtime::closed))]
    Closed,
}
