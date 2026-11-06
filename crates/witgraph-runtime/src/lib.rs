//! Component-model graph runtime for witgraph.
//!
//! This crate extends the witgraph typestate chain with [`RuntimeGraph`],
//! which loads a [`CompiledGraph`](witgraph_ir::CompiledGraph) together
//! with WASM component bytes and runs it on wasmtime.
//!
//! ```text
//! GraphBuilder -> Graph -> CompiledGraph -> RuntimeGraph
//! ```
//!
//! Every node is a component whose world is its contract: an exported
//! `node` interface with up to two records, `inputs` and `outputs` (at least
//! one), and a `run` function. A graph is composed into one component, with a
//! generated hub that holds its values, so they move from node to node without
//! passing through the host. The host calls each node's `run` through the hub, on
//! the task that calls [`RuntimeGraph::tick`], and reads and writes ports through
//! typed exports of the hub when it chooses ([`Port`], [`RuntimeGraph::preview`]).
//! One composite is one Store: any fault stops the whole graph. See [`engine`] and
//! [`graph`] for the invariants.
//!
//! # Crate layout
//!
//! The scheduling itself (ticks, stream groups, feedback) is [`witgraph_sched`],
//! which knows no engine and holds no value; this crate is its wasmtime data
//! plane.
//!
//! - [`graph`] — [`RuntimeGraph`], [`RuntimeConfig`], [`PreparedGraph`], [`Port`].
//! - [`engine`] — the Store; the embedder's [`Host`] and its Store data
//!   ([`StoreData`], [`HostState`]).
//! - [`plugin`] — a [`Host`] made of capability providers ([`Plugins`],
//!   [`CapabilityPlugin`]).
//! - [`error`] — [`LoadError`].
//!
//! The scheduler's types a [`RuntimeGraph`] hands out are re-exported at the
//! crate root: a node's lifecycle state ([`NodeState`], [`NodePhase`]),
//! instrumentation ([`RuntimeMode`], [`Perf`], [`Trace`], [`TraceEvent`]),
//! [`TickResult`], [`FaultReport`], snapshots ([`Snapshot`]), and the errors
//! [`RuntimeError`] and [`NodeFault`].
//!
//! The crate's public API is built on [`wasmtime`] (its `Engine`, `Linker`
//! and component `Val` and `Type`) and [`witgraph_ir`]; both are
//! re-exported, so an embedder can use exactly the versions it was built
//! against.

pub mod engine;
pub mod error;
pub mod graph;
mod holds;
#[cfg(feature = "debug")]
pub mod lines;
pub(crate) mod plane;
pub mod plugin;
pub mod shared;

pub use engine::{CapabilityGap, Host, HostState, NoCapabilities, StoreData};
pub use error::LoadError;
#[cfg(feature = "compile")]
pub use graph::LinkedProvider;
pub use graph::{
    ByteCount, Fuel, Handle, HandlePort, Input, InputPort, Output, OutputPort, Port, PreparedGraph,
    Preview, RuntimeConfig, RuntimeGraph, ValReader, Value,
};
pub use holds::RuntimeHolds;
#[cfg(feature = "debug")]
pub use lines::RuntimeLines;
pub use plugin::{CapabilityPlugin, Extensions, PluginData, Plugins};
pub use shared::Shared;
pub use wasmtime::component::Val;
pub use witgraph_sched::{
    FaultReport, NodeFault, NodePhase, NodeState, Perf, PortValues, PortVersion, RuntimeError,
    RuntimeMode, Snapshot, SnapshotError, TickResult, Trace, TraceEvent,
};
pub use witgraph_traits::holds::{HeldRun, HoldAt, HoldId, NodeHolds};
#[cfg(feature = "debug")]
pub use witgraph_traits::lines::{
    BreakpointScope, LineDebugger, LineStop, OwnLines, OwnRules, PathPattern, SourceFile,
    SourceLanguage, SourceVariable, StackFrame, StopReason,
};
pub use witgraph_traits::{OutputReader, RunningGraph, until};
pub use {wasmtime, witgraph_ir, witgraph_sched, witgraph_traits};
