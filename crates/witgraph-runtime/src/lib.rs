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
//! one), and a `run` function. Nodes joined by stream/future connections
//! form an *island*. Members whose handles the host cannot move share a
//! wasmtime Store, so those handles pass directly between them; members
//! joined only by streams of scalars or strings get a Store each, and the
//! host pumps the items across. Value connections between islands go
//! through the host. Islands run concurrently, in generations, on the task
//! that calls [`RuntimeGraph::tick`]. The island is the unit with a
//! lifecycle (a typestate); a node's [`NodeState`] is a view of its
//! island's. See [`engine`] and [`graph`] for the invariants.
//!
//! # Crate layout
//!
//! The scheduling itself (ticks, generations, latched Values, feedback,
//! snapshots) is [`witgraph_sched`], which knows no engine; this crate is
//! its wasmtime executor.
//!
//! - [`graph`] — [`RuntimeGraph`], [`RuntimeConfig`], [`PreparedComponent`].
//! - [`engine`] — island Stores and the generation driver; the embedder's
//!   [`Host`] and its Store data ([`IslandData`], [`HostState`]).
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
pub mod plugin;
pub(crate) mod pump;

pub use engine::{CapabilityGap, Host, HostState, IslandData, NoCapabilities};
pub use error::LoadError;
pub use graph::{LinkedProvider, PreparedComponent, RuntimeConfig, RuntimeGraph};
pub use plugin::{CapabilityPlugin, Extensions, PluginData, Plugins};
pub use wasmtime::component::Val;
pub use witgraph_sched::{
    FaultReport, IslandSnapshot, NodeFault, NodePhase, NodeState, Perf, PortValues, RuntimeError,
    RuntimeMode, Snapshot, TickResult, Trace, TraceEvent,
};
pub use {wasmtime, witgraph_ir, witgraph_sched};
