//! The witgraph scheduler, independent of any WebAssembly engine.
//!
//! [`SchedulerCore`] drives a [`CompiledGraph`](witgraph_ir::CompiledGraph): it
//! decides when each node runs, routes what changed to who owes a run, latches
//! feedback once per tick, and holds no value. A [`DataPlane`] holds them, and
//! runs the nodes: `witgraph-runtime` keeps them in a component composed with the
//! nodes, on wasmtime, and `witgraph-web` the same on a JavaScript host. What the
//! two do with their hub is [`hub`]. Because this crate has no engine in it, it also
//! builds for `wasm32`.
//!
//! # Crate layout
//!
//! - [`control`] — [`SchedulerCore`] and the scheduling model.
//! - [`hub`] — writes, clears, snapshots and restores, over [`HubPorts`].
//! - [`executor`] — the seam: [`DataPlane`], [`Command`].
//! - [`node`] — a node's lifecycle state ([`NodeState`], [`NodePhase`]).
//! - [`mode`] — instrumentation ([`RuntimeMode`], [`Perf`], [`Trace`],
//!   [`TraceEvent`]).
//! - [`schedule`] — [`TickResult`], [`FaultReport`].

pub mod control;
pub mod hub;
pub use witgraph_traits::{error, executor, mode, node, schedule, snapshot};
pub(crate) mod lifecycle;
#[cfg(test)]
mod model_tests;
pub(crate) mod resource;
#[cfg(test)]
mod tests;

pub use control::{
    DEFAULT_MAX_STEPS_PER_TICK, PortVersions, SchedulerCore, check_max_steps_per_tick,
    check_resources,
};
pub use hub::{
    HubPorts, PortSlot, ReturnedNumbers, decode_returned, latched_output, not_a_value_output,
};
pub use witgraph_ir;
pub use witgraph_traits::*;
