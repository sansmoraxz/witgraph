//! Snapshots of the host-visible state.

use std::collections::{BTreeMap, BTreeSet};

use witgraph_ir::{ComponentRef, Connection, ConnectionId, Link, NodeId, PortName};

use crate::node::NodePhase;

/// WAVE text per port, keyed by node id then port name.
pub type PortValues = BTreeMap<NodeId, BTreeMap<PortName, String>>;

/// The host-visible state of a runtime graph at one point in time, for
/// point-in-time restores and debugging. Produced by
/// `snapshot` (at any time), applied by
/// `restore`.
///
/// Values are WAVE text, parsed against the port types on restore. Only
/// what the host sees is captured: latched Values, pending feedback, node
/// phases, and the inputs each in-flight or queued generation starts with.
/// Guest memory (including suspended tasks) is not captured either.
///
/// Snapshots do not store stream or future contents. Restoring re-runs any
/// generation that was in flight, which recreates its streams from the
/// recorded inputs; the result matches the original only if the guests are
/// deterministic.
///
/// Nor do they store handles, which are the host's and not text: a latched
/// handle is left out, and the island that produced it is recorded as
/// queued, so after a restore it runs again and makes a new one. A
/// generation in flight that started with a handle is recorded as queued
/// instead of replayed: it runs once its producers have delivered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Each node's resolved component (id and content hash). A snapshot
    /// restores only onto a graph with exactly these nodes and components.
    pub nodes: BTreeMap<NodeId, ComponentRef>,
    /// The graph's connections, sorted by id. A snapshot restores only onto
    /// a graph wired exactly the same way.
    pub connections: Vec<Connection>,
    /// The graph's links, sorted by id. A snapshot restores only onto a
    /// graph whose nodes are linked to the same providers, by ref. Like
    /// nodes, providers are named, not their bytes, and a provider's content
    /// hash is not checked against its bytes (it has no contract to hash):
    /// a pinned ref only selects the key its bytes are loaded under. Keep
    /// the bytes behind a provider's key unchanged for a restore to replay
    /// the same code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Whether no generation was in flight when the snapshot was taken. A
    /// replay owed after a restore is not in flight, so it is listed in
    /// [`islands`](Self::islands) while this is `true`.
    pub quiescent: bool,
    /// Every node's phase when the snapshot was taken. Informational:
    /// `restore` does not re-fault or re-cancel islands.
    pub phases: BTreeMap<NodeId, NodePhase>,
    /// Latched Value inputs: injected, delivered by connections, or latched
    /// from feedback.
    pub inputs: PortValues,
    /// Latched Value outputs: what each node's `run` returned last
    /// (including `run`s of an in-flight generation that already returned).
    pub outputs: PortValues,
    /// Feedback values waiting for the next latch, by connection, as the
    /// source port produced them.
    pub feedback: BTreeMap<ConnectionId, String>,
    /// Islands with an in-flight or queued generation.
    pub islands: Vec<IslandSnapshot>,
}

/// An island with work outstanding when a [`Snapshot`] was taken.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IslandSnapshot {
    /// The island's members, sorted. `restore` matches
    /// islands by their set of members, in any order.
    pub members: Vec<NodeId>,
    /// The external Value inputs the in-flight generation started with, by
    /// member, or those of a restored replay not started yet; `None` when
    /// there is neither.
    pub running: Option<PortValues>,
    /// Whether another generation was queued (an input changed since the
    /// last start). It runs with the latched inputs.
    pub queued: bool,
    /// The feedback connections out of this island whose target the host
    /// wrote after the [`running`](Self::running) generation started: what
    /// that generation feeds back there is stale, and its replay drops it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub stale_feedback: BTreeSet<ConnectionId>,
}

/// Why a snapshot could not be taken: a value it holds could not be written
/// as WAVE text (`Executor::to_wave`, for instance).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{at}: {message}")]
pub struct SnapshotError {
    /// Which value: an output or input (`` `node.port` ``), a feedback
    /// connection, or an input a generation started with.
    pub at: String,
    /// Why it could not be written.
    pub message: String,
}
