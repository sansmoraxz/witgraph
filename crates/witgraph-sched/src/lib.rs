//! The witgraph scheduler, independent of any WebAssembly engine.
//!
//! A [`Scheduler`] drives a [`CompiledGraph`](witgraph_ir::CompiledGraph):
//! it keeps the latched Value of every port, decides when each island runs
//! a generation, routes outputs, latches feedback once per tick, and takes
//! and restores snapshots. It never touches a guest itself. An
//! [`Executor`] does: it instantiates the nodes, runs a generation when
//! asked, and defines what a value is.
//!
//! `witgraph-runtime` is the wasmtime executor. Because this crate has no
//! engine in it, it also builds for `wasm32`, where an executor can run
//! the same graphs, under the same rules, on a JavaScript host.
//!
//! # Crate layout
//!
//! - [`scheduler`] — [`Scheduler`] and the scheduling model.
//! - [`executor`] — the seam: [`Executor`], [`PortValue`], [`Generation`],
//!   and the in-island loop ([`drive`], [`NodeCaller`]).
//! - [`plan`] — static island wiring ([`IslandPlan`], [`RunShape`]).
//! - [`snapshot`] — [`Snapshot`].
//! - [`node`] — a node's lifecycle state ([`NodeState`], [`NodePhase`]).
//! - [`mode`] — instrumentation ([`RuntimeMode`], [`Perf`], [`Trace`],
//!   [`TraceEvent`]).
//! - [`schedule`] — [`TickResult`], [`FaultReport`].
//! - [`error`] — [`RuntimeError`], [`NodeFault`].

pub mod error;
pub mod executor;
pub(crate) mod island;
pub mod mode;
pub mod node;
pub mod plan;
pub(crate) mod resource;
mod run;
pub mod schedule;
pub mod scheduler;
pub mod snapshot;
#[cfg(test)]
mod tests;

pub use error::{NodeFault, RuntimeError};
pub use executor::{
    Executor, Generation, GenerationFuture, IslandEvent, IslandEventKind, MaybeSend, MaybeSync,
    NodeCaller, OptionPayload, OptionPayloadFn, Outcome, PortValue, Reporter, drive,
    missing_input_message,
};
pub use mode::{Perf, RuntimeMode, Trace, TraceEvent};
pub use node::{NodePhase, NodeState};
pub use plan::{IslandPlan, MemberPlan, RunShape};
pub use schedule::{FaultReport, TickResult};
pub use scheduler::{
    DEFAULT_MAX_STEPS_PER_TICK, Scheduler, check_max_steps_per_tick, check_resources,
};
pub use snapshot::{IslandSnapshot, PortValues, Snapshot};
pub use witgraph_ir;
