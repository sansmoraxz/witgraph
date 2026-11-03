//! Lifecycles as typestates: each phase owns exactly the data meaningful in it,
//! and a transition consumes one phase and returns the next, so a state the
//! lifecycle does not allow cannot be represented. Each thing that has a
//! lifecycle keeps it in a dispatch enum, since a table's entries differ.

use std::task::{Context, Poll};

use futures::FutureExt;
use futures::channel::mpsc::UnboundedSender;
use witgraph_ir::NodeId;
use witgraph_traits::{BoxFuture, Command, NodeFault, NodePhase, Outcome};

/// Nothing to do. Whether it has run since the graph was built is what tells
/// [`Pending`](NodePhase::Pending) from [`Idle`](NodePhase::Idle).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Waiting {
    ran: bool,
}

/// Its group has started, and it will run once what it reads has settled.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scheduled;

/// Its `run` is in flight.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Running;

/// A node in phase `S`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Node<S>(S);

impl Node<Waiting> {
    pub(crate) fn fresh() -> Self {
        Self(Waiting { ran: false })
    }

    pub(crate) fn schedule(self) -> Node<Scheduled> {
        Node(Scheduled)
    }

    pub(crate) fn start(self) -> Node<Running> {
        Node(Running)
    }
}

impl Node<Scheduled> {
    pub(crate) fn start(self) -> Node<Running> {
        Node(Running)
    }
}

impl Node<Running> {
    pub(crate) fn returned(self) -> Node<Waiting> {
        Node(Waiting { ran: true })
    }
}

/// A node, in whichever phase it is in.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NodeLife {
    Waiting(Node<Waiting>),
    Scheduled(Node<Scheduled>),
    Running(Node<Running>),
}

impl NodeLife {
    pub(crate) fn phase(self) -> NodePhase {
        match self {
            Self::Waiting(Node(Waiting { ran: false })) => NodePhase::Pending,
            Self::Waiting(Node(Waiting { ran: true })) => NodePhase::Idle,
            Self::Scheduled(_) => NodePhase::Pending,
            Self::Running(_) => NodePhase::Running,
        }
    }

    pub(crate) fn is_running(self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub(crate) fn has_run(self) -> bool {
        matches!(self, Self::Waiting(Node(Waiting { ran: true })))
    }
}

/// A stream group with nothing in flight.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Quiet;

/// A stream group some of whose `run`s are yet to return.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Active {
    left: usize,
}

/// A stream group whose `run`s have all returned, and whose writers may not
/// have finished: it holds its claims, and cannot start again, until no guest
/// work is left.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Draining;

/// A stream group in phase `S`. Only a group has [`Draining`], so a Value-only
/// node cannot wait on the Store, and a group cannot skip it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Group<S>(S);

impl Group<Quiet> {
    pub(crate) fn fresh() -> Self {
        Self(Quiet)
    }

    pub(crate) fn start(self, members: usize) -> Group<Active> {
        Group(Active { left: members })
    }
}

impl Group<Active> {
    /// A member returned: the group is still active, or draining.
    pub(crate) fn returned(self) -> GroupLife {
        match self.0.left {
            0 | 1 => GroupLife::Draining(Group(Draining)),
            left => GroupLife::Active(Group(Active { left: left - 1 })),
        }
    }
}

impl Group<Draining> {
    pub(crate) fn drained(self) -> Group<Quiet> {
        Group(Quiet)
    }
}

/// A stream group, in whichever phase it is in.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GroupLife {
    Quiet(Group<Quiet>),
    Active(Group<Active>),
    Draining(Group<Draining>),
}

/// Why a graph has no instance.
#[derive(Debug, Clone)]
pub(crate) enum StopCause {
    /// A node or rebuild faulted; the node that caused it, when it is known.
    Faulted(NodeFault, Option<NodeId>),
    /// The host shut it down.
    Shutdown,
}

/// An instance is up.
pub(crate) struct Live {
    commands: UnboundedSender<Command>,
    server: BoxFuture<Outcome>,
}

/// No instance: the next run builds one.
pub(crate) struct Stopped {
    cause: StopCause,
}

/// A graph's instance, in phase `S`. Only [`Live`] has a way to ask anything of
/// one.
pub(crate) struct Instance<S>(S);

impl Instance<Live> {
    pub(crate) fn new(commands: UnboundedSender<Command>, server: BoxFuture<Outcome>) -> Self {
        Self(Live { commands, server })
    }

    pub(crate) fn send(&self, command: Command) {
        // The server is held here, so it hears everything sent, until it ends.
        let _ = self.0.commands.unbounded_send(command);
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Outcome> {
        self.0.server.poll_unpin(cx)
    }

    /// Drops the server, and with it the instance.
    pub(crate) fn stop(self, cause: StopCause) -> Instance<Stopped> {
        Instance(Stopped { cause })
    }
}

impl Instance<Stopped> {
    pub(crate) fn cause(&self) -> &StopCause {
        &self.0.cause
    }

    pub(crate) fn restart(
        self,
        commands: UnboundedSender<Command>,
        server: BoxFuture<Outcome>,
    ) -> Instance<Live> {
        Instance::new(commands, server)
    }
}

/// A graph's instance, in whichever phase it is in.
pub(crate) enum GraphState {
    Live(Instance<Live>),
    Stopped(Instance<Stopped>),
}
