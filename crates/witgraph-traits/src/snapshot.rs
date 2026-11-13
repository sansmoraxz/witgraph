//! Snapshots of the host-visible state.

use std::collections::{BTreeMap, BTreeSet};

use witgraph_ir::{Connection, ConnectionId, ContractId, Link, NodeId, PortName};

use crate::node::NodePhase;

/// WAVE text per port, keyed by node id then port name.
pub type PortValues = BTreeMap<NodeId, BTreeMap<PortName, String>>;

/// The host-visible state of a runtime graph at one point in time, for
/// point-in-time restores and debugging.
///
/// Values are WAVE text, parsed against the port types on restore. Only what
/// the host sees is captured: latched Values, pending feedback, node phases, and
/// the runs in flight or owed. Guest memory (including suspended tasks) is not
/// captured.
///
/// Snapshots do not store stream or future contents. Restoring re-runs what was
/// in flight, on the latched inputs, which recreates its streams; the result
/// matches the original only if the guests are deterministic.
///
/// Nor do they store handles, which are the host's and not text: a latched
/// handle is left out, and the node that produced it is recorded as owing a run,
/// so after a restore it makes a new one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// What each node runs: its world, as its component provides it. A
    /// snapshot restores only onto a graph with exactly these nodes running
    /// these. Components are named, not their bytes: keep the components
    /// behind the names unchanged for a restore to replay the same code.
    pub nodes: BTreeMap<NodeId, ContractId>,
    /// The graph's connections, sorted by id. A snapshot restores only onto
    /// a graph wired exactly the same way.
    pub connections: Vec<Connection>,
    /// The graph's links, sorted by id. A snapshot restores only onto a
    /// graph whose nodes are linked to the same providers, by name. Like
    /// nodes, providers are named, not their bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Whether no `run` was in flight when the snapshot was taken.
    pub quiescent: bool,
    /// Every node's phase when the snapshot was taken. Informational: a restore
    /// does not fault or cancel anything.
    pub phases: BTreeMap<NodeId, NodePhase>,
    /// Latched Value inputs: injected, delivered by connections, or latched
    /// from feedback.
    pub inputs: PortValues,
    /// Latched Value outputs: what each node's `run` returned last.
    pub outputs: PortValues,
    /// Feedback values waiting for the next latch, by connection, as the
    /// source port produced them.
    pub feedback: BTreeMap<ConnectionId, String>,
    /// The inputs the host wrote, of those with a value: what outlives an
    /// instance. Values latched from feedback are the graph's, and start over.
    pub host_written: BTreeMap<NodeId, BTreeSet<PortName>>,
    /// The nodes with a `run` in flight.
    pub in_flight: BTreeSet<NodeId>,
    /// The nodes owing a run.
    pub owed: BTreeSet<NodeId>,
}

/// Why a snapshot could not be taken: a value it holds could not be written
/// as WAVE text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{at}: {message}")]
pub struct SnapshotError {
    /// Which value: an output or input (`` `node.port` ``), or a feedback
    /// connection.
    pub at: String,
    /// Why it could not be written.
    pub message: String,
}
