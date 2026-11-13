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
pub use composed::{ComposedGraph, HubError, HubMember, HubOptions, compose_graph};
use witgraph_ir::{FeedbackIdx, Idx, InputIdx, OutputIdx};

/// The interface the host drives a hub with.
pub const CONTROL_INTERFACE: &str = "witgraph:hub/control@0.1.0";
/// The interface holding a hub's functions per port.
pub const PORTS_INTERFACE: &str = "witgraph:hub/ports@0.1.0";
/// The interface a hub imports to show the host a handle.
pub const LEND_INTERFACE: &str = "witgraph:hub/lend@0.1.0";
/// The interface a hub with stream taps ([`HubOptions::stream_taps`]) imports to show the host
/// what its streams carry.
pub const PREVIEW_INTERFACE: &str = "witgraph:hub/preview@0.1.0";

/// A function of [`CONTROL_INTERFACE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFunc {
    /// `run: async func(member: u32) -> returned`.
    Run,
    /// `clear-input: func(input: u32) -> bool`.
    ClearInput,
    /// `latch-feedback: func(connection: u32) -> latched`.
    LatchFeedback,
    /// `drop-feedback: func(connection: u32)`.
    DropFeedback,
}

impl ControlFunc {
    /// Every one of them.
    pub const ALL: [ControlFunc; 4] = [
        ControlFunc::Run,
        ControlFunc::ClearInput,
        ControlFunc::LatchFeedback,
        ControlFunc::DropFeedback,
    ];

    /// Its name in WIT.
    pub const fn name(self) -> &'static str {
        match self {
            ControlFunc::Run => "run",
            ControlFunc::ClearInput => "clear-input",
            ControlFunc::LatchFeedback => "latch-feedback",
            ControlFunc::DropFeedback => "drop-feedback",
        }
    }
}

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

/// The name of the function tapping a stream input, with [`HubOptions::stream_taps`]:
/// `() -> u32`. The stream its writer gave the input stays in the hub, and the input holds a
/// new one in its place, which [`pump_stream`] feeds: the tap it returns, or 0 for none.
pub fn tap_stream(input: InputIdx) -> String {
    format!("tap-stream-{}", input.position())
}

/// The name of the function moving a tap's items on to its reader, telling the host of each
/// chunk through [`stream_items`]: `async (tap: u32)`, returning once either end is dropped.
pub fn pump_stream(input: InputIdx) -> String {
    format!("pump-stream-{}", input.position())
}

/// The name of the function a hub imports from [`PREVIEW_INTERFACE`], told of what a tapped
/// stream input carries, a chunk at a time: `(items: list<T>)`.
pub fn stream_items(input: InputIdx) -> String {
    format!("stream-items-{}", input.position())
}

/// The name of the function tapping a stream output nothing reads, with
/// [`HubOptions::stream_taps`]: `() -> u32`. The hub keeps the stream its writer returned, in
/// place of dropping it, for [`pump_output`] to read: the tap it returns, or 0 for none.
pub fn tap_output(output: OutputIdx) -> String {
    format!("tap-output-{}", output.position())
}

/// The name of the function reading a tapped output's items, telling the host of each chunk
/// through [`output_items`]: `async (tap: u32)`, returning once its writer is done.
pub fn pump_output(output: OutputIdx) -> String {
    format!("pump-output-{}", output.position())
}

/// The name of the function a hub imports from [`PREVIEW_INTERFACE`], told of what a tapped
/// stream output nothing reads carries, a chunk at a time: `(items: list<T>)`.
pub fn output_items(output: OutputIdx) -> String {
    format!("output-items-{}", output.position())
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

/// What a composite's import is, by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeImport<'a> {
    /// A member's `node` interface ([`member_import`]), when it is mocked.
    Member(usize),
    /// A node's `witgraph:runtime/host` ([`host_import`]).
    Host(usize),
    /// Anything else: [`LEND_INTERFACE`], [`PREVIEW_INTERFACE`], or a capability.
    Other(&'a str),
}

impl<'a> CompositeImport<'a> {
    /// The inverse of [`member_import`] and [`host_import`]. A capability may be
    /// named like a member (`m0`): only the caller knows which nodes are mocked.
    pub fn parse(name: &'a str) -> Self {
        let number = |text: &str| text.parse::<usize>().ok();
        let host = name
            .strip_prefix("node-")
            .and_then(|rest| rest.strip_suffix("-host"))
            .and_then(number);
        if let Some(node) = host {
            return Self::Host(node);
        }
        match name.strip_prefix('m').and_then(number) {
            Some(node) => Self::Member(node),
            None => Self::Other(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_import_name_parses_back() {
        assert_eq!(
            CompositeImport::parse(&member_import(3)),
            CompositeImport::Member(3)
        );
        assert_eq!(
            CompositeImport::parse(&host_import(7)),
            CompositeImport::Host(7)
        );
        for other in [
            LEND_INTERFACE,
            "mqtt",
            "node-x-host",
            "m",
            "m1x",
            "test:a/b@1.0.0",
        ] {
            assert_eq!(CompositeImport::parse(other), CompositeImport::Other(other));
        }
    }
}
