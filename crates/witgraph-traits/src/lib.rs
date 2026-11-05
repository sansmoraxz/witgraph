//! What witgraph's hosts have in common.
//!
//! Every host runs a graph the same way, on wasmtime or under JavaScript, so
//! this crate holds the one API: [`RunningGraph`] and [`OutputReader`] are
//! what a running graph offers, and [`DataPlane`] are what
//! runs one. `witgraph-runtime` and `witgraph-web` each implement them.
//! Host-specific things (how a value is represented) are associated types, so
//! the calls have one shape, even where one host could answer at once.

pub mod error;
pub mod executor;
pub mod graph;
pub mod lines;
pub mod mode;
pub mod node;
pub mod schedule;
pub mod snapshot;

pub use error::{NodeFault, RuntimeError};
pub use executor::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, Incarnation, Latched, MaybeSend,
    MaybeSync, Outcome, Returned, Serving, boxed,
};
pub use graph::{OutputReader, PortVersion, RunningGraph, until};
pub use mode::{Perf, RuntimeMode, Trace, TraceEvent};
pub use node::{NodePhase, NodeState};
pub use schedule::{FaultReport, TickResult};
pub use snapshot::{PortValues, Snapshot, SnapshotError};
pub use witgraph_ir;
