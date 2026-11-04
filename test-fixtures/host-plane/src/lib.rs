//! A data plane that keeps every value in the host, beside an [`Executor`] of
//! closures: the scheduler with no engine at all, for tests and to measure what
//! scheduling alone costs. No host runs on it: wasmtime and JavaScript keep their
//! values in a hub ([`witgraph_sched::hub`]).

pub mod executor;
pub mod latches;
pub mod plan;
pub mod scheduler;
pub mod snapshot;
#[cfg(test)]
mod tests;

pub use executor::{CallFuture, Canonical, Executor, OptionPayload, OptionPayloadFn, PortValue};
pub use latches::LatchReader;
pub use plan::{MemberPlan, RunShape};
pub use scheduler::Scheduler;
pub use witgraph_traits::*;
