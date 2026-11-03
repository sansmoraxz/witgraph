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
//! - [`latches`] — the values, held in the host ([`LatchReader`]).
//! - [`executor`] — the seams: [`DataPlane`], [`Executor`], [`PortValue`],
//!   [`Generation`], and the in-island loop ([`drive`], [`NodeCaller`]).
//! - [`plan`] — static island wiring ([`IslandPlan`], [`RunShape`]).
//! - [`snapshot`] — [`Snapshot`].
//! - [`node`] — a node's lifecycle state ([`NodeState`], [`NodePhase`]).
//! - [`mode`] — instrumentation ([`RuntimeMode`], [`Perf`], [`Trace`],
//!   [`TraceEvent`]).
//! - [`schedule`] — [`TickResult`], [`FaultReport`].
//! - [`error`] — [`RuntimeError`], [`NodeFault`].

pub(crate) mod control;
pub use witgraph_traits::{error, mode, node, schedule};
pub mod executor;
pub(crate) mod island;
pub mod latches;
pub mod plan;
pub(crate) mod resource;
mod run;
pub mod scheduler;
pub mod snapshot;
#[cfg(test)]
mod tests;

pub use control::{DEFAULT_MAX_STEPS_PER_TICK, check_max_steps_per_tick, check_resources};
pub use executor::{NodeCaller, drive, missing_input_message};
pub use latches::LatchReader;
pub use scheduler::Scheduler;
pub use snapshot::SnapshotError;
pub use witgraph_ir;
pub use witgraph_traits::*;
