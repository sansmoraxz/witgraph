//! Event-driven WASM graph runtime for witgraph.
//!
//! This crate extends the witgraph typestate chain with
//! [`RuntimeGraph`], which loads a [`CompiledGraph`](witgraph_ir::CompiledGraph)
//! together with WASM component bytes and executes it reactively.
//! Every node is a WASM component; execution is event-driven, with
//! independent nodes running concurrently.
//!
//! # Typestate chain
//!
//! ```text
//! GraphBuilder -> Graph -> CompiledGraph -> RuntimeGraph
//! ```
//!
//! # Crate layout
//!
//! - [`Val`] — Runtime value enum (re-exported from [`witgraph_ir`]).
//! - [`channel`] — Port interconnection channels ([`Channel`],
//!   [`ValueSlot`], [`EventQueue`],
//!   [`StreamChannel`], [`FutureSlot`]).
//! - [`error`] — Error types ([`RuntimeError`], [`NodeFault`],
//!   [`ChannelError`]).
//! - [`abi`] — Activation interface ([`Activation`], [`ActivationResult`],
//!   [`InputSnapshot`], [`OutputCollector`]).
//! - [`node`] — Node lifecycle as a typestate machine ([`Node`],
//!   [`NodeState`], [`NodePhase`]).
//! - [`mode`] — Runtime mode ([`RuntimeMode`], [`Release`],
//!   [`struct@Debug`]).
//! - [`schedule`] — Scheduler events ([`SchedulerEvent`], [`TickResult`]).
//! - [`engine`] — Wasmtime engine
//!   ([`engine::WasmEngine`], [`engine::NodeInstance`]).
//! - [`graph`] — Runtime graph ([`RuntimeGraph`], [`RuntimeConfig`]).

pub mod abi;
pub mod channel;
pub mod engine;
pub mod error;
pub mod graph;
pub mod mode;
pub mod node;
pub(crate) mod resource;
pub mod schedule;

// Re-exports for convenience.
pub use abi::{Activation, ActivationResult, InputSnapshot, OutputCollector, OutputWrite};
pub use channel::{Channel, EventQueue, FutureSlot, StreamChannel, StreamPull, ValueSlot};
pub use engine::{val_to_bytes, NodeHostState};
pub use error::{ChannelError, NodeFault, RuntimeError};
pub use graph::{RuntimeConfig, RuntimeGraph, ValuesSnapshot};
pub use mode::{Debug, Release, RuntimeMode, TraceEvent};
pub use node::{Node, NodePhase, NodeState};
pub use schedule::{SchedulerEvent, TickResult};
pub use witgraph_ir::Val;
