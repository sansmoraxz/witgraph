//! The hub: the component that holds a graph's values and moves them between
//! its nodes, inside one composition, so they never pass through the host.
//!
//! It is generated per graph. It imports every node's `node` interface under a
//! name of its own, and exports `witgraph:hub/control`, the fixed interface the
//! host drives it with, and `witgraph:hub/ports`, one function per port with the
//! port's real type, so that a typed host call is type-checked by the engine.
//!
//! A return's record stays where it landed, and slots point into it: delivery
//! copies nothing. The real copies are the two the canonical ABI makes per hop.
//! What a return owns is freed when the last slot or call holding it lets go,
//! by walking its type, since `cabi_realloc` is not told which call it serves.

#[cfg(feature = "compose")]
mod abi;
#[cfg(feature = "compose")]
mod composed;
#[cfg(feature = "compose")]
mod generate;
#[cfg(feature = "compose")]
mod module;
#[cfg(feature = "compose")]
mod text;

#[cfg(feature = "compose")]
pub use composed::{ComposedGraph, HubError, HubMember, compose_graph};
use witgraph_ir::{FeedbackIdx, Idx, InputIdx, OutputIdx};

/// The interface the host drives a hub with.
pub const CONTROL_INTERFACE: &str = "witgraph:hub/control@0.1.0";
/// The interface holding a hub's functions per port.
pub const PORTS_INTERFACE: &str = "witgraph:hub/ports@0.1.0";
/// The interface a hub imports to show the host a handle.
pub const LEND_INTERFACE: &str = "witgraph:hub/lend@0.1.0";

/// The name of the function reading an output: `() -> option<T>`.
pub fn get_output(output: OutputIdx) -> String {
    format!("get-output-{}", output.position())
}

/// The name of the function writing an output, without delivering it:
/// `(value: list<T>) -> bool`, the list holding the one value.
pub fn set_output(output: OutputIdx) -> String {
    format!("set-output-{}", output.position())
}

/// The name of the function reading an input: `() -> option<T>`.
pub fn get_input(input: InputIdx) -> String {
    format!("get-input-{}", input.position())
}

/// The name of the function writing an input: `(value: list<T>) -> bool`, the
/// list holding the one value, returning whether it changed anything.
pub fn set_input(input: InputIdx) -> String {
    format!("set-input-{}", input.position())
}

/// The name of the function reading a buffered feedback value:
/// `() -> option<T>`.
pub fn get_feedback(connection: FeedbackIdx) -> String {
    format!("get-feedback-{}", connection.position())
}

/// The name of the function buffering a feedback value:
/// `(value: list<T>) -> bool`.
pub fn set_feedback(connection: FeedbackIdx) -> String {
    format!("set-feedback-{}", connection.position())
}

/// The name of the function taking a stream or future input's handle out of the
/// hub: `() -> option<stream<T>>`. So a host can watch what passes, and close it.
pub fn take_stream(input: InputIdx) -> String {
    format!("take-stream-{}", input.position())
}

/// The name of the function putting a stream or future in an input:
/// `(value: stream<T>)`.
pub fn give_stream(input: InputIdx) -> String {
    format!("give-stream-{}", input.position())
}

/// The name of the function lending the host a handle output, through the
/// import of [`lend_handle`]: `()`.
pub fn read_handle(output: OutputIdx) -> String {
    format!("read-handle-{}", output.position())
}

/// The name of the function a hub imports from [`LEND_INTERFACE`] to lend a
/// handle output: `(it: borrow<R>)`.
pub fn lend_handle(output: OutputIdx) -> String {
    format!("lend-handle-{}", output.position())
}

/// The name the hub imports a member's `node` interface under. A mocked node
/// (see [`HubMember::mocked`]) leaves it an import of the composite, for the host.
pub fn member_import(node: usize) -> String {
    format!("m{node}")
}

/// The name a node's `witgraph:runtime/host` is imported under by the composed
/// graph, `node` the node's position in the graph's
/// [`PortTable`](witgraph_ir::PortTable).
pub fn host_import(node: usize) -> String {
    format!("node-{node}-host")
}
