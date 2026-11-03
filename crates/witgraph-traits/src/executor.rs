//! What runs a graph: the seams between the scheduler and an engine.
//!
//! The scheduler decides *when* a node runs. A [`DataPlane`] runs it, and holds
//! every value, so the scheduler never sees one. An [`Executor`] is the smaller
//! seam of a data plane that keeps its values in the host.

use std::borrow::Cow;
use std::sync::Arc;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use witgraph_ir::{
    FeedbackIdx, InputIdx, NodeId, NodeIdx, OutputIdx, PortDirection, PortName, PortRef,
};

use crate::error::NodeFault;
use crate::plan::MemberPlan;

/// `Send` on native targets, nothing on WebAssembly: there everything is
/// driven by JavaScript promises, which never leave their thread.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send` on native targets, nothing on WebAssembly: there everything is
/// driven by JavaScript promises, which never leave their thread.
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}
#[cfg(target_family = "wasm")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets, nothing on WebAssembly (see [`MaybeSend`]).
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_family = "wasm"))]
impl<T: Sync + ?Sized> MaybeSync for T {}

/// `Sync` on native targets, nothing on WebAssembly (see [`MaybeSend`]).
#[cfg(target_family = "wasm")]
pub trait MaybeSync {}
#[cfg(target_family = "wasm")]
impl<T: ?Sized> MaybeSync for T {}

/// A boxed future: `Send` on native targets.
#[cfg(not(target_family = "wasm"))]
pub type BoxFuture<T> = futures::future::BoxFuture<'static, T>;

/// A boxed future: `Send` on native targets.
#[cfg(target_family = "wasm")]
pub type BoxFuture<T> = futures::future::LocalBoxFuture<'static, T>;

/// Boxes `future`, as [`BoxFuture`] is on this target.
#[cfg(not(target_family = "wasm"))]
pub fn boxed<T>(future: impl Future<Output = T> + Send + 'static) -> BoxFuture<T> {
    Box::pin(future)
}

/// Boxes `future`, as [`BoxFuture`] is on this target.
#[cfg(target_family = "wasm")]
pub fn boxed<T>(future: impl Future<Output = T> + 'static) -> BoxFuture<T> {
    Box::pin(future)
}

/// A value a port carries, as a host-held data plane needs it: cloned to fan
/// out, compared to decide whether an input changed. Implemented for every
/// type that qualifies; what an option holds is the executor's to say
/// ([`Executor::option_payload`]).
pub trait PortValue: Clone + PartialEq + MaybeSend + MaybeSync + 'static {}
impl<T: Clone + PartialEq + MaybeSend + MaybeSync + 'static> PortValue for T {}

/// A value in the one form a guest produces, so that equal values compare
/// equal: value equality decides whether an input changed. The write path takes
/// nothing else.
///
/// Made by an executor's [`check_input`](Executor::check_input) and
/// [`parse_wave`](Executor::parse_wave), the only places that know a value's
/// canonical form.
#[derive(Debug, Clone, PartialEq)]
#[repr(transparent)]
pub struct Canonical<V>(V);

impl<V> Canonical<V> {
    /// Vouches that `value` is in its canonical form. For an executor's
    /// `check_input` and `parse_wave`; nothing checks it.
    pub fn assume(value: V) -> Self {
        Self(value)
    }

    /// The value.
    pub fn into_inner(self) -> V {
        self.0
    }
}

/// What a value is to a connection that unwraps an option
/// ([`Executor::option_payload`]).
#[derive(Debug, Clone, PartialEq)]
pub enum OptionPayload<'a, V: Clone> {
    /// Not an option: the connection delivers the value itself.
    NotOption,
    /// `some`: the connection delivers its payload. Borrowed where the
    /// value holds its payload as a value of its own.
    Some(Cow<'a, V>),
    /// `none`: the input is absent.
    None,
}

/// What a connection that unwraps an option delivers for a value
/// ([`Executor::option_payload`]).
pub type OptionPayloadFn<V> = for<'a> fn(&'a V) -> OptionPayload<'a, V>;

/// How serving an instance ended: the fault that stopped it, with the node
/// that caused it, when that is known.
pub type Outcome = Result<(), (NodeFault, Option<NodeId>)>;

/// What a `run` returning changed, as the scheduler needs it: which ports,
/// never their values. So the scheduler can tell when to run what, wherever
/// the values are held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Returned {
    /// The outputs that hold a new value.
    pub outputs_changed: Vec<OutputIdx>,
    /// The inputs that now hold a different value, and whose node owes a run.
    pub inputs_changed: Vec<InputIdx>,
    /// The inputs written inside a stream group: they hold a value now, and
    /// their node, which runs with the group, owes nothing.
    pub inputs_recorded: Vec<InputIdx>,
    /// The inputs that held a value and no longer do.
    pub inputs_cleared: Vec<InputIdx>,
    /// The feedback connections holding a new value for the next latch.
    pub feedback_buffered: Vec<FeedbackIdx>,
}

/// What latching a feedback connection did to its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latched {
    /// It holds the value it held.
    Unchanged,
    /// It holds another value.
    Changed,
    /// It held a value and no longer does.
    Cleared,
}

/// Which building of the graph's instance this is: what stops a report of an
/// instance that is gone being taken for one of the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct Incarnation(u64);

impl Incarnation {
    /// The one after this.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What the scheduler asks of an instance. They are served in order, but for
/// [`Run`](Self::Run), whose calls overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Calls a node's `run`, on its latched inputs.
    Run(NodeIdx),
    /// Writes a feedback connection's buffered value to its input. Answered
    /// with [`ControlKind::Latched`].
    LatchFeedback(FeedbackIdx),
    /// Forgets a feedback connection's buffered value.
    DropFeedback(FeedbackIdx),
    /// Asks to be told when no guest work is left. Answered, with the number
    /// it carries, by [`ControlKind::Quiet`].
    AwaitQuiet(u64),
}

/// A report from an instance.
#[derive(Debug)]
pub struct ControlEvent {
    /// The instance reporting.
    pub incarnation: Incarnation,
    /// What happened.
    pub kind: ControlKind,
}

/// What happened in an instance.
#[derive(Debug)]
pub enum ControlKind {
    /// It was built afresh, and its first command is being served.
    Built,
    /// A node's `run` returned.
    RunReturned {
        /// The node.
        node: NodeIdx,
        /// What changed.
        returned: Returned,
    },
    /// A [`Command::LatchFeedback`] was served.
    Latched {
        /// The connection.
        connection: FeedbackIdx,
        /// What it did.
        latched: Latched,
    },
    /// No guest work was left when a [`Command::AwaitQuiet`] was served.
    Quiet(u64),
    /// Items of a node's stream output passed through the data plane on their
    /// way to the stream's reader, with stream items asked for.
    StreamItems {
        /// The node whose output the stream is.
        node: NodeIdx,
        /// The output port.
        port: PortName,
        /// How many items passed.
        count: usize,
    },
}

/// What is told of a new instance.
#[derive(Debug)]
pub struct Serving {
    /// Which one it is.
    pub incarnation: Incarnation,
    /// What it is asked.
    pub commands: UnboundedReceiver<Command>,
    /// Where it reports.
    pub events: UnboundedSender<ControlEvent>,
    /// Whether stream items are wanted ([`ControlKind::StreamItems`]).
    pub stream_items: bool,
}

/// What holds a graph's values and runs its nodes, as the scheduler sees it.
/// The scheduler decides when; through this it never sees a value.
pub trait DataPlane: MaybeSend + 'static {
    /// A future serving a new instance of the graph, built afresh unless one
    /// was built ahead for its first: every command, until the commands
    /// end, or a fault stops it. Dropping it stops the instance. A failed build
    /// is the fault [`NodeFault::Restart`], or [`NodeFault::Fatal`] when a start
    /// function called `fatal`.
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome>;

    /// The instance is gone, with every value it held: all that survives are
    /// the inputs the host wrote, written to the next one.
    fn stopped(&self);
}

/// What a call of a node's `run` ends with: its outputs, by field name.
pub type CallFuture<V> = BoxFuture<Result<Vec<(String, V)>, (NodeFault, Option<NodeId>)>>;

/// What runs the nodes of a graph whose values the host keeps.
pub trait Executor: MaybeSend + MaybeSync + 'static {
    /// The values Value ports carry.
    type Value: PortValue;

    /// The types of Value ports.
    type Type: 'static;

    /// Calls a node's `run`. `args` holds one value per field of its `inputs`
    /// record, in declaration order (`None` for an absent one).
    fn call(
        &self,
        member: Arc<MemberPlan>,
        args: Vec<Option<Self::Value>>,
    ) -> CallFuture<Self::Value>;

    /// Gives every node fresh guest state.
    fn rebuild(&self) -> BoxFuture<Outcome>;

    /// Drops a stream or future output nothing reads, so the guest's writes
    /// fail instead of blocking.
    fn close(&self, value: Self::Value);

    /// An instance stopped with calls of `nodes` still in flight: they belong
    /// to nothing now.
    fn abandon(&self, nodes: &[NodeId]);

    /// What a connection that unwraps an option delivers for `value`: the
    /// payload of `some`, nothing for `none` (an absent optional input),
    /// and the value itself when it is not an option.
    fn option_payload(value: &Self::Value) -> OptionPayload<'_, Self::Value>;

    /// The type of `port` when it is a Value port of that direction: for an
    /// input, its payload type (for an optional port, the inner type);
    /// `None` otherwise.
    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&Self::Type>;

    /// Checks that `value` has type `ty` (a Value input's payload type) and
    /// returns it in the one form a guest produces, so equal values compare
    /// equal.
    fn check_input(
        &self,
        ty: &Self::Type,
        value: Self::Value,
    ) -> Result<Canonical<Self::Value>, String>;

    /// Parses WAVE text as a value of type `ty` (a Value port's), in the
    /// same form as [`check_input`](Self::check_input) returns. Record
    /// fields, cases and flags the type does not have are errors.
    fn parse_wave(&self, ty: &Self::Type, text: &str) -> Result<Canonical<Self::Value>, String>;

    /// A value of a Value port as WAVE text, or why it cannot be written
    /// (one too large for the host to convert, say).
    fn to_wave(&self, value: &Self::Value) -> Result<String, String>;
}
