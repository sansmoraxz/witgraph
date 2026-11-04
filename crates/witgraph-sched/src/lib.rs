//! The witgraph scheduler, independent of any WebAssembly engine.
//!
//! [`SchedulerCore`] drives a [`CompiledGraph`](witgraph_ir::CompiledGraph): it
//! decides when each node runs, routes what changed to who owes a run, latches
//! feedback once per tick, and holds no value. A [`DataPlane`] holds them, and
//! runs the nodes. On wasmtime, `witgraph-runtime` keeps them in a component
//! composed with the nodes.
//!
//! A host whose values are not in the guest uses [`Scheduler`], which keeps them
//! in the host beside an [`Executor`] that calls the nodes. Because this crate
//! has no engine in it, it also builds for `wasm32`, where an executor can run
//! the same graphs, under the same rules, on a JavaScript host.
//!
//! # Crate layout
//!
//! - [`scheduler`] — [`Scheduler`] and the scheduling model.
//! - [`latches`] — the values, held in the host ([`LatchReader`]).
//! - [`snapshot`] — [`Snapshot`].
//! - [`executor`] — the seams: [`DataPlane`], [`Executor`], [`Command`].
//! - [`plan`] — how to call a node ([`MemberPlan`], [`RunShape`]).
//! - [`node`] — a node's lifecycle state ([`NodeState`], [`NodePhase`]).
//! - [`mode`] — instrumentation ([`RuntimeMode`], [`Perf`], [`Trace`],
//!   [`TraceEvent`]).
//! - [`schedule`] — [`TickResult`], [`FaultReport`].
//! - [`error`] — [`RuntimeError`], [`NodeFault`].

pub mod control;
pub mod hub;
pub use witgraph_traits::{error, executor, mode, node, plan, schedule};
pub mod latches;
pub(crate) mod lifecycle;
#[cfg(test)]
mod model_tests;
pub(crate) mod resource;
pub mod scheduler;
pub mod snapshot;
#[cfg(test)]
mod tests;

pub use control::{
    DEFAULT_MAX_STEPS_PER_TICK, PortVersions, SchedulerCore, check_max_steps_per_tick,
    check_resources,
};
pub use hub::{HubPorts, PortSlot, latched_output, not_a_value_output};
pub use latches::LatchReader;
pub use scheduler::Scheduler;
pub use snapshot::SnapshotError;
pub use witgraph_ir;
pub use witgraph_traits::*;
