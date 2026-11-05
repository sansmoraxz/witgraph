//! What runs a graph: the seams between the scheduler and an engine.
//!
//! The scheduler decides *when* a node runs. A [`DataPlane`] runs it, and holds
//! every value, so the scheduler never sees one.

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use witgraph_ir::{FeedbackIdx, InputIdx, NodeId, NodeIdx, OutputIdx};

use crate::error::NodeFault;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A stream group's `run`s have all returned: tell of when the guest work they left
    /// is over, by [`ControlKind::Drained`], whatever else goes on beside it. A host that
    /// cannot tell whose work is whose ignores this, and the group drains at the next
    /// [`ControlKind::Quiet`], which an endless stream anywhere can put off for good.
    AwaitDrained {
        /// Which group, in [`CompiledGraph::stream_groups`](witgraph_ir::CompiledGraph::stream_groups).
        group: usize,
        /// The number to answer with.
        at: u64,
        /// Its nodes.
        members: Box<[NodeIdx]>,
    },
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
    /// The guest work a [`Command::AwaitDrained`] asked of is over.
    Drained {
        /// Its group.
        group: usize,
        /// Its number.
        at: u64,
    },
    /// A write the scheduler was told of as it started reached the input.
    HostWrote {
        /// The input.
        input: InputIdx,
        /// Whether that changed its value.
        changed: bool,
    },
    /// A clear the scheduler was told of as it started reached the input.
    HostCleared {
        /// The input.
        input: InputIdx,
        /// Whether it had a value.
        cleared: bool,
    },
    /// What a restore writes into a new instance has been written.
    Restored,
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
