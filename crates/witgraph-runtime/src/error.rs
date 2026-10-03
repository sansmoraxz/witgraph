//! Error types for the witgraph runtime.
//!
//! Two tiers: [`RuntimeError`] for failures the host sees when loading or
//! driving the graph, and [`NodeFault`] for a failure inside an island,
//! which faults every node of that island.

use witgraph_ir::{ComponentRef, ConnectionId, NodeId, PortDirection, PortName};

/// A graph-level runtime error: loading, or a host call with bad arguments.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum RuntimeError {
    /// A component has no corresponding WASM bytes.
    #[error(
        "no WASM component provided for `{component}`{}",
        component.content_hash.as_ref().map(|h| format!(" (content hash {h})")).unwrap_or_default()
    )]
    #[diagnostic(code(witgraph::runtime::missing_wasm))]
    MissingWasm {
        /// The component missing its bytes.
        component: Box<ComponentRef>,
    },
    /// A component's bytes are not a valid component, embed WIT that could
    /// not be decoded or lowered into exactly one node contract, or failed
    /// to compile on the engine.
    #[error("component `{component}` does not describe a witgraph node: {message}")]
    #[diagnostic(code(witgraph::runtime::bad_component))]
    BadComponent {
        /// The component whose bytes were rejected.
        component: Box<ComponentRef>,
        /// What went wrong.
        message: String,
    },
    /// A component's bytes implement a different contract than the one the
    /// graph was compiled against: other ports or `run` kind, or an import
    /// the contract does not declare as a capability.
    #[error("component `{component}` does not match its contract: {message}")]
    #[diagnostic(code(witgraph::runtime::contract_mismatch))]
    ContractMismatch {
        /// The component whose bytes were rejected.
        component: Box<ComponentRef>,
        /// The first difference found.
        message: String,
    },
    /// A node failed to link or instantiate (a missing capability import,
    /// a trapping start function, an island over its memory limit), or
    /// [`Host::island_data`](crate::Host::island_data) failed for its
    /// island.
    #[error("failed to instantiate node `{node}`: {message}")]
    #[diagnostic(code(witgraph::runtime::instantiation))]
    Instantiation {
        /// The node whose component could not be instantiated.
        node: NodeId,
        /// The underlying error message.
        message: String,
    },
    /// A restore was attempted while a generation is in flight. Restore
    /// onto a freshly loaded graph, or after `shutdown`/`cancel`.
    #[error("a generation is in flight; restore needs a graph with nothing in flight")]
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
    /// [`clear_input`]) or Value output (for [`read_output`]) of the node.
    ///
    /// [`inject`]: crate::RuntimeGraph::inject
    /// [`clear_input`]: crate::RuntimeGraph::clear_input
    /// [`read_output`]: crate::RuntimeGraph::read_output
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
    /// [`inject`](crate::RuntimeGraph::inject) targeted an input that a
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

/// Why an island faulted. Every node of the island carries the same fault;
/// a trap poisons the island's whole Store.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum NodeFault {
    /// A component in the island trapped (spawned tasks included).
    #[error("WASM trap: {message}")]
    #[diagnostic(code(witgraph::runtime::wasm_trap))]
    WasmTrap {
        /// The trap message.
        message: String,
    },
    /// Growing a linear memory or table would have taken the island past
    /// [`RuntimeConfig::max_island_memory`](crate::RuntimeConfig::max_island_memory).
    #[error("the island needs {requested} bytes of memory, over its limit of {limit}")]
    #[diagnostic(code(witgraph::runtime::memory_limit))]
    MemoryLimit {
        /// The bytes the island would have held after the growth.
        requested: usize,
        /// The island's limit.
        limit: usize,
    },
    /// One call copied more out of a guest than
    /// [`RuntimeConfig::hostcall_fuel`](crate::RuntimeConfig::hostcall_fuel)
    /// allows.
    #[error("a call copied more data out of the guest than `hostcall_fuel` allows")]
    #[diagnostic(code(witgraph::runtime::hostcall_fuel))]
    HostcallFuelExhausted,
    /// The island burned its whole fuel budget
    /// ([`RuntimeConfig::fuel_per_run`](crate::RuntimeConfig::fuel_per_run))
    /// before its next `run` started.
    #[error("fuel exhausted")]
    #[diagnostic(code(witgraph::runtime::fuel_exhausted))]
    FuelExhausted,
    /// A node called `witgraph:runtime/host.fatal`. The tick that saw it
    /// returned [`TickResult::Aborted`](crate::TickResult::Aborted).
    #[error("fatal: {message}")]
    #[diagnostic(code(witgraph::runtime::fatal))]
    Fatal {
        /// The message passed to `fatal`.
        message: String,
    },
    /// Rebuilding the stopped island, at the start of its next generation,
    /// failed: a member failed to instantiate, or
    /// [`Host::island_data`](crate::Host::island_data) failed.
    #[error("restart failed: {message}")]
    #[diagnostic(code(witgraph::runtime::restart))]
    Restart {
        /// Why the island could not be rebuilt.
        message: String,
    },
}
