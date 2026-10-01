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
//! form an *island* that shares one wasmtime Store, so stream and future
//! handles pass directly between them; Value connections between islands go
//! through the host. Islands run concurrently, in generations, on the task
//! that calls [`RuntimeGraph::tick`]. The island is the unit with a
//! lifecycle (a typestate); a node's [`NodeState`] is a view of its
//! island's. See [`engine`] and [`graph`] for the invariants.
//!
//! # Crate layout
//!
//! - [`graph`] — [`RuntimeGraph`], [`RuntimeConfig`], [`Snapshot`].
//! - [`engine`] — island Stores and the generation driver ([`HostState`]).
//! - [`node`] — a node's lifecycle state, projected from its island
//!   ([`NodeState`], [`NodePhase`]).
//! - [`mode`] — instrumentation ([`RuntimeMode`], [`Release`],
//!   [`struct@Debug`], [`TraceEvent`]).
//! - [`schedule`] — [`TickResult`].
//! - [`error`] — [`RuntimeError`], [`NodeFault`].

pub mod engine;
pub mod error;
pub mod graph;
pub(crate) mod island;
pub mod mode;
pub mod node;
pub(crate) mod resource;
pub mod schedule;

pub use engine::HostState;
pub use error::{NodeFault, RuntimeError};
pub use graph::{IslandSnapshot, PortValues, RuntimeConfig, RuntimeGraph, Snapshot};
pub use mode::{Debug, Release, RuntimeMode, TraceEvent};
pub use node::{NodePhase, NodeState};
pub use schedule::TickResult;
pub use wasmtime::component::Val;
