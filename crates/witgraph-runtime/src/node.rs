//! Node lifecycle as a typestate machine.
//!
//! A node's lifecycle is modelled by [`Node<S>`], where the marker type
//! `S` is the phase the node is in. Each phase owns only the data that is
//! meaningful in it (a [`Suspended`] node holds its saved output, a
//! [`Faulted`] node holds its cause), and every transition is a method
//! that consumes the node and returns it in its next phase. A transition
//! the lifecycle does not allow has no method, so it does not compile.
//!
//! ```text
//!   Created --init--> Draining --(drains done)--> Ready
//!      \                  \                         |
//!       `----init----------`--------------------> Ready
//!
//!   Ready | Draining --activate--> Running
//!   Running --proceed--> Draining | Ready
//!   Running --complete--> Completed
//!   Running --suspend--> Suspended --resume--> Running
//!
//!   any live phase --fault--> Faulted
//!   any live phase --cancel--> Cancelled --restart--> Created
//! ```
//!
//! The scheduler keeps nodes in a table keyed by [`NodeId`] and their
//! phase changes in response to runtime events, so the table needs one
//! heterogeneous element type: [`NodeState`]. It is a plain container
//! with no transition logic of its own; for a read-only view of where a
//! node is, [`NodeState::phase`] projects it onto the fieldless
//! [`NodePhase`].

use witgraph_ir::{ConnectionId, ConsumptionMode, NodeId};

use crate::abi::{ActivationResult, OutputWrite};
use crate::error::NodeFault;

mod sealed {
    pub trait Sealed {}
}

/// A lifecycle phase marker. Sealed: the set of phases is fixed.
pub trait Phase: sealed::Sealed {
    /// The fieldless [`NodePhase`] this marker corresponds to.
    const PHASE: NodePhase;
}

/// A phase from which a node can still be faulted or cancelled: every
/// phase except the terminal ones. Sealed.
pub trait Live: Phase {}

/// Allocated, but `init()` has not yet been called.
#[derive(Debug)]
pub struct Created;

/// `init()` has run; drained inputs are being consumed to completion.
#[derive(Debug)]
pub struct Draining;

/// Waiting for an activation trigger.
#[derive(Debug)]
pub struct Ready;

/// Currently inside `activate()`.
#[derive(Debug)]
pub struct Running;

/// Blocked on downstream backpressure, holding the output it could not
/// commit.
#[derive(Debug)]
pub struct Suspended {
    out: SuspendedOutput,
}

/// Finished successfully. Terminal.
#[derive(Debug)]
pub struct Completed;

/// Stopped by an unrecoverable error. Terminal.
#[derive(Debug)]
pub struct Faulted {
    cause: NodeFault,
}

/// Cancelled by the host or by upstream propagation. Terminal, though the
/// scheduler can restart a cancelled node when new input arrives.
#[derive(Debug)]
pub struct Cancelled;

macro_rules! impl_phase {
    ($($ty:ident => $phase:ident),* $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl Phase for $ty {
            const PHASE: NodePhase = NodePhase::$phase;
        }
    )*};
}

impl_phase! {
    Created => Created,
    Draining => Draining,
    Ready => Ready,
    Running => Running,
    Suspended => Suspended,
    Completed => Completed,
    Faulted => Faulted,
    Cancelled => Cancelled,
}

impl Live for Created {}
impl Live for Draining {}
impl Live for Ready {}
impl Live for Running {}
impl Live for Suspended {}

/// Saved output state for a node suspended due to downstream channel
/// backpressure.
///
/// When a producer's output commit encounters a full bounded channel,
/// the uncommitted writes are captured here. On resumption (after the
/// consumer frees capacity), the scheduler retries the commit without
/// re-executing the node's WASM activation.
#[derive(Debug)]
pub(crate) struct SuspendedOutput {
    /// The activation result from the original WASM activation, applied
    /// once all writes finally commit.
    pub(crate) activation_result: ActivationResult,
    /// The connection whose channel was at capacity, triggering
    /// suspension.
    pub(crate) blocked_conn: ConnectionId,
    /// Fan-out connections still pending for the first entry in
    /// `writes`. Subsequent entries use the full fan-out set from
    /// `output_map`.
    pub(crate) pending_conns: Vec<ConnectionId>,
    /// The uncommitted output writes. The first entry is partially
    /// committed (only `pending_conns` remain); subsequent entries are
    /// fully uncommitted.
    pub(crate) writes: Vec<OutputWrite>,
}

/// A node in lifecycle phase `S`.
///
/// Holds the data common to every phase. Phase-specific data lives in
/// the marker `S`, and transitions are inherent methods available only in
/// the phases that allow them.
#[derive(Debug)]
pub struct Node<S> {
    id: NodeId,
    mode: ConsumptionMode,
    /// Drained inputs that have not yet completed. Non-zero from
    /// creation until the last drain finishes, across `Draining`,
    /// `Running` and `Suspended`. A [`Ready`] node always has none; the
    /// constructors of `Ready` and `Draining` enforce that.
    pending_drains: usize,
    state: S,
}

impl<S> Node<S> {
    /// The node's identity.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// How the node consumes its inputs.
    pub fn mode(&self) -> ConsumptionMode {
        self.mode
    }

    /// The number of drained inputs still awaiting completion.
    pub fn pending_drains(&self) -> usize {
        self.pending_drains
    }

    /// Re-tags the node with the next phase's state.
    fn into_phase<T>(self, state: T) -> Node<T> {
        Node {
            id: self.id,
            mode: self.mode,
            pending_drains: self.pending_drains,
            state,
        }
    }
}

impl<S: Phase> Node<S> {
    /// The phase this node is in.
    pub fn phase(&self) -> NodePhase {
        S::PHASE
    }
}

/// Transitions available from every live phase.
impl<S: Live> Node<S> {
    /// Faults the node, recording why.
    pub(crate) fn fault(self, cause: NodeFault) -> Node<Faulted> {
        self.into_phase(Faulted { cause })
    }

    /// Cancels the node. Anything it was holding for the current phase
    /// (such as a suspended output) is dropped.
    pub(crate) fn cancel(self) -> Node<Cancelled> {
        self.into_phase(Cancelled)
    }
}

impl Node<Created> {
    /// Creates a node that has not been initialized yet.
    ///
    /// `pending_drains` is the number of drained input ports in its
    /// contract.
    pub(crate) fn new(id: NodeId, mode: ConsumptionMode, pending_drains: usize) -> Self {
        Self {
            id,
            mode,
            pending_drains,
            state: Created,
        }
    }

    /// `init()` succeeded. The node starts draining if it has drained
    /// inputs, otherwise it is ready.
    pub(crate) fn initialized(self) -> NodeState {
        if self.pending_drains > 0 {
            self.into_phase(Draining).into()
        } else {
            self.into_phase(Ready).into()
        }
    }
}

impl Node<Ready> {
    /// The scheduler activates the node.
    pub(crate) fn activate(self) -> Node<Running> {
        self.into_phase(Running)
    }
}

impl Node<Draining> {
    /// The scheduler activates the node to consume drained inputs.
    pub(crate) fn activate(self) -> Node<Running> {
        self.into_phase(Running)
    }
}

impl Node<Running> {
    /// One drained input finished (stream closed and emptied, or future
    /// resolved).
    pub(crate) fn complete_drain(&mut self) {
        self.pending_drains = self.pending_drains.saturating_sub(1);
    }

    /// The activation returned [`ActivationResult::Continue`]. The node
    /// goes back to draining if drains remain, otherwise to ready.
    pub(crate) fn proceed(self) -> NodeState {
        if self.pending_drains > 0 {
            self.into_phase(Draining).into()
        } else {
            self.into_phase(Ready).into()
        }
    }

    /// The activation returned [`ActivationResult::Completed`].
    pub(crate) fn complete(self) -> Node<Completed> {
        self.into_phase(Completed)
    }

    /// The activation's outputs could not all be committed because a
    /// downstream channel is full.
    pub(crate) fn suspend(self, out: SuspendedOutput) -> Node<Suspended> {
        self.into_phase(Suspended { out })
    }
}

impl Node<Suspended> {
    /// The connection whose full channel the node is waiting on.
    pub(crate) fn blocked_on(&self) -> &ConnectionId {
        &self.state.out.blocked_conn
    }

    /// Resumes the node to retry committing the saved output.
    pub(crate) fn resume(self) -> (Node<Running>, SuspendedOutput) {
        let out = self.state.out;
        let running = Node {
            id: self.id,
            mode: self.mode,
            pending_drains: self.pending_drains,
            state: Running,
        };
        (running, out)
    }
}

impl Node<Faulted> {
    /// Why the node faulted.
    pub fn cause(&self) -> &NodeFault {
        &self.state.cause
    }
}

impl Node<Cancelled> {
    /// Re-instantiates a cancelled node: it starts over from
    /// [`Created`] with a fresh drain count.
    pub(crate) fn restart(self, pending_drains: usize) -> Node<Created> {
        Node {
            id: self.id,
            mode: self.mode,
            pending_drains,
            state: Created,
        }
    }
}

/// The fieldless projection of a node's phase, for tracing and for
/// asserting on where a node is.
///
/// This is a read-only view derived from the node's type. It is not
/// where the lifecycle is enforced; see [`Node`] for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodePhase {
    /// Allocated but not yet initialized.
    Created,
    /// Drained inputs are being consumed to completion.
    Draining,
    /// Waiting for an activation trigger.
    Ready,
    /// Currently inside `activate()`.
    Running,
    /// Waiting on downstream backpressure.
    Suspended,
    /// Finished successfully.
    Completed,
    /// Unrecoverable error.
    Faulted,
    /// Cancelled by the host.
    Cancelled,
}

impl NodePhase {
    /// Returns `true` if this phase is a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NodePhase::Completed | NodePhase::Faulted | NodePhase::Cancelled
        )
    }
}

/// A node in whichever phase it is currently in.
///
/// This is the element type of the scheduler's node table. It holds no
/// transition logic: scheduler code matches on it to get the typed
/// [`Node<S>`] for a phase, and applies that phase's transitions.
#[derive(Debug)]
pub enum NodeState {
    /// See [`Created`].
    Created(Node<Created>),
    /// See [`Draining`].
    Draining(Node<Draining>),
    /// See [`Ready`].
    Ready(Node<Ready>),
    /// See [`Running`].
    Running(Node<Running>),
    /// See [`Suspended`].
    Suspended(Node<Suspended>),
    /// See [`Completed`].
    Completed(Node<Completed>),
    /// See [`Faulted`].
    Faulted(Node<Faulted>),
    /// See [`Cancelled`].
    Cancelled(Node<Cancelled>),
}

macro_rules! each_phase {
    ($state:expr, $n:ident => $body:expr) => {
        match $state {
            NodeState::Created($n) => $body,
            NodeState::Draining($n) => $body,
            NodeState::Ready($n) => $body,
            NodeState::Running($n) => $body,
            NodeState::Suspended($n) => $body,
            NodeState::Completed($n) => $body,
            NodeState::Faulted($n) => $body,
            NodeState::Cancelled($n) => $body,
        }
    };
}

macro_rules! impl_from_node {
    ($($ty:ident),* $(,)?) => {$(
        impl From<Node<$ty>> for NodeState {
            fn from(node: Node<$ty>) -> Self {
                NodeState::$ty(node)
            }
        }
    )*};
}

impl_from_node!(
    Created, Draining, Ready, Running, Suspended, Completed, Faulted, Cancelled
);

impl NodeState {
    /// The node's identity.
    pub fn id(&self) -> &NodeId {
        each_phase!(self, n => n.id())
    }

    /// How the node consumes its inputs.
    pub fn mode(&self) -> ConsumptionMode {
        each_phase!(self, n => n.mode())
    }

    /// The number of drained inputs still awaiting completion.
    pub fn pending_drains(&self) -> usize {
        each_phase!(self, n => n.pending_drains())
    }

    /// The phase the node is in.
    pub fn phase(&self) -> NodePhase {
        each_phase!(self, n => n.phase())
    }

    /// Returns `true` if the node has reached a terminal phase.
    pub fn is_terminal(&self) -> bool {
        self.phase().is_terminal()
    }

    /// Why the node faulted, if it is [`Faulted`].
    pub fn fault_cause(&self) -> Option<&NodeFault> {
        match self {
            NodeState::Faulted(n) => Some(n.cause()),
            _ => None,
        }
    }

    /// Faults a live node. A node that is already terminal is returned
    /// unchanged: the first terminal phase wins.
    pub(crate) fn fault(self, cause: NodeFault) -> NodeState {
        match self {
            NodeState::Created(n) => n.fault(cause).into(),
            NodeState::Draining(n) => n.fault(cause).into(),
            NodeState::Ready(n) => n.fault(cause).into(),
            NodeState::Running(n) => n.fault(cause).into(),
            NodeState::Suspended(n) => n.fault(cause).into(),
            terminal @ (NodeState::Completed(_)
            | NodeState::Faulted(_)
            | NodeState::Cancelled(_)) => terminal,
        }
    }

    /// Cancels a live node. A node that is already terminal is returned
    /// unchanged.
    pub(crate) fn cancel(self) -> NodeState {
        match self {
            NodeState::Created(n) => n.cancel().into(),
            NodeState::Draining(n) => n.cancel().into(),
            NodeState::Ready(n) => n.cancel().into(),
            NodeState::Running(n) => n.cancel().into(),
            NodeState::Suspended(n) => n.cancel().into(),
            terminal @ (NodeState::Completed(_)
            | NodeState::Faulted(_)
            | NodeState::Cancelled(_)) => terminal,
        }
    }

    /// Starts an activation: a [`Ready`] or [`Draining`] node becomes
    /// [`Running`].
    ///
    /// A node that is already `Running` is returned as-is, which is what
    /// a scheduler tick dropped mid-activation leaves behind.
    pub(crate) fn activate(self) -> NodeState {
        match self {
            NodeState::Ready(n) => n.activate().into(),
            NodeState::Draining(n) => n.activate().into(),
            running @ NodeState::Running(_) => running,
            other => misuse(other, "Ready or Draining"),
        }
    }

    /// Applies `f` to a [`Created`] node.
    pub(crate) fn map_created(self, f: impl FnOnce(Node<Created>) -> NodeState) -> NodeState {
        match self {
            NodeState::Created(n) => f(n),
            other => misuse(other, "Created"),
        }
    }

    /// Applies `f` to a [`Running`] node.
    pub(crate) fn map_running(self, f: impl FnOnce(Node<Running>) -> NodeState) -> NodeState {
        match self {
            NodeState::Running(n) => f(n),
            other => misuse(other, "Running"),
        }
    }

    /// Applies `f` to a [`Suspended`] node.
    pub(crate) fn map_suspended(self, f: impl FnOnce(Node<Suspended>) -> NodeState) -> NodeState {
        match self {
            NodeState::Suspended(n) => f(n),
            other => misuse(other, "Suspended"),
        }
    }

    /// Applies `f` to a [`Cancelled`] node.
    pub(crate) fn map_cancelled(self, f: impl FnOnce(Node<Cancelled>) -> NodeState) -> NodeState {
        match self {
            NodeState::Cancelled(n) => f(n),
            other => misuse(other, "Cancelled"),
        }
    }
}

/// A transition was requested from a phase that does not allow it.
///
/// The scheduler is expected to know which phase a node is in, so this
/// is a bug: it trips a debug assertion, and in release builds the node
/// is left as it was.
fn misuse(state: NodeState, expected: &str) -> NodeState {
    debug_assert!(
        false,
        "node `{}` is {:?}, expected {expected}",
        state.id(),
        state.phase(),
    );
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(drains: usize) -> Node<Created> {
        Node::new("sensor".into(), ConsumptionMode::Sync, drains)
    }

    fn fault() -> NodeFault {
        NodeFault::WasmTrap {
            message: "boom".into(),
        }
    }

    fn suspended_output() -> SuspendedOutput {
        SuspendedOutput {
            activation_result: ActivationResult::Continue,
            blocked_conn: "c1".into(),
            pending_conns: vec!["c1".into()],
            writes: Vec::new(),
        }
    }

    #[test]
    fn new_node_is_created() {
        let node = created(0);
        assert_eq!(node.phase(), NodePhase::Created);
        assert_eq!(node.id(), &NodeId::from("sensor"));
        assert_eq!(node.mode(), ConsumptionMode::Sync);
        assert_eq!(node.pending_drains(), 0);
    }

    #[test]
    fn init_without_drains_is_ready() {
        let state = created(0).initialized();
        assert_eq!(state.phase(), NodePhase::Ready);
    }

    #[test]
    fn init_with_drains_is_draining() {
        let state = created(2).initialized();
        assert_eq!(state.phase(), NodePhase::Draining);
        assert_eq!(state.pending_drains(), 2);
    }

    #[test]
    fn ready_node_runs_then_returns_to_ready() {
        let NodeState::Ready(node) = created(0).initialized() else {
            panic!("expected Ready");
        };
        let running = node.activate();
        assert_eq!(running.phase(), NodePhase::Running);
        assert_eq!(running.proceed().phase(), NodePhase::Ready);
    }

    #[test]
    fn running_node_completes() {
        let NodeState::Ready(node) = created(0).initialized() else {
            panic!("expected Ready");
        };
        let done = node.activate().complete();
        assert_eq!(done.phase(), NodePhase::Completed);
        assert!(NodeState::from(done).is_terminal());
    }

    #[test]
    fn drains_survive_activation_and_finish_in_ready() {
        let NodeState::Draining(node) = created(2).initialized() else {
            panic!("expected Draining");
        };

        // First activation consumes one of two drains: still draining.
        let mut running = node.activate();
        running.complete_drain();
        let NodeState::Draining(node) = running.proceed() else {
            panic!("expected Draining");
        };
        assert_eq!(node.pending_drains(), 1);

        // Second activation consumes the last: now ready.
        let mut running = node.activate();
        running.complete_drain();
        let state = running.proceed();
        assert_eq!(state.phase(), NodePhase::Ready);
        assert_eq!(state.pending_drains(), 0);
    }

    #[test]
    fn complete_drain_saturates() {
        let NodeState::Ready(node) = created(0).initialized() else {
            panic!("expected Ready");
        };
        let mut running = node.activate();
        running.complete_drain();
        assert_eq!(running.pending_drains(), 0);
    }

    #[test]
    fn suspend_holds_output_and_resume_returns_it() {
        let NodeState::Draining(node) = created(1).initialized() else {
            panic!("expected Draining");
        };
        let suspended = node.activate().suspend(suspended_output());
        assert_eq!(suspended.phase(), NodePhase::Suspended);
        assert_eq!(suspended.blocked_on(), &ConnectionId::from("c1"));
        // Drain progress is carried across suspension.
        assert_eq!(suspended.pending_drains(), 1);

        let (running, out) = suspended.resume();
        assert_eq!(running.phase(), NodePhase::Running);
        assert_eq!(running.pending_drains(), 1);
        assert_eq!(out.blocked_conn, ConnectionId::from("c1"));
        assert_eq!(out.activation_result, ActivationResult::Continue);
    }

    #[test]
    fn every_live_phase_can_be_cancelled_and_faulted() {
        fn live_states() -> Vec<NodeState> {
            let ready = match created(0).initialized() {
                NodeState::Ready(n) => n,
                _ => unreachable!(),
            };
            let running = match created(0).initialized() {
                NodeState::Ready(n) => n.activate(),
                _ => unreachable!(),
            };
            let suspended = match created(0).initialized() {
                NodeState::Ready(n) => n.activate().suspend(suspended_output()),
                _ => unreachable!(),
            };
            vec![
                created(0).into(),
                created(1).initialized(),
                ready.into(),
                running.into(),
                suspended.into(),
            ]
        }

        for state in live_states() {
            let phase = state.phase();
            assert!(!state.is_terminal(), "{phase:?} should be live");
            assert_eq!(state.cancel().phase(), NodePhase::Cancelled, "{phase:?}");
        }
        for state in live_states() {
            let phase = state.phase();
            let faulted = state.fault(fault());
            assert_eq!(faulted.phase(), NodePhase::Faulted, "{phase:?}");
            assert!(faulted.fault_cause().is_some());
        }
    }

    #[test]
    fn faulted_node_keeps_its_cause() {
        let state = created(0).fault(fault());
        assert!(matches!(
            NodeState::from(state).fault_cause(),
            Some(NodeFault::WasmTrap { message }) if message == "boom"
        ));
    }

    #[test]
    fn terminal_phases_ignore_fault_and_cancel() {
        let NodeState::Ready(node) = created(0).initialized() else {
            panic!("expected Ready");
        };
        let completed: NodeState = node.activate().complete().into();
        assert_eq!(completed.fault(fault()).phase(), NodePhase::Completed);

        let faulted: NodeState = created(0).fault(fault()).into();
        assert_eq!(faulted.cancel().phase(), NodePhase::Faulted);

        let cancelled: NodeState = created(0).cancel().into();
        assert_eq!(cancelled.fault(fault()).phase(), NodePhase::Cancelled);
    }

    #[test]
    fn cancelled_node_restarts_from_created() {
        let cancelled = created(1).cancel();
        let restarted = cancelled.restart(3);
        assert_eq!(restarted.phase(), NodePhase::Created);
        assert_eq!(restarted.pending_drains(), 3);
        assert_eq!(restarted.id(), &NodeId::from("sensor"));
    }

    #[test]
    fn activate_is_idempotent_on_running() {
        let NodeState::Ready(node) = created(0).initialized() else {
            panic!("expected Ready");
        };
        let running: NodeState = node.activate().into();
        assert_eq!(running.activate().phase(), NodePhase::Running);
    }

    #[test]
    #[should_panic(expected = "expected Ready or Draining")]
    #[cfg(debug_assertions)]
    fn activating_a_created_node_is_a_bug() {
        let _ = NodeState::from(created(0)).activate();
    }

    #[test]
    fn terminal_phases() {
        assert!(NodePhase::Completed.is_terminal());
        assert!(NodePhase::Faulted.is_terminal());
        assert!(NodePhase::Cancelled.is_terminal());
        for phase in [
            NodePhase::Created,
            NodePhase::Draining,
            NodePhase::Ready,
            NodePhase::Running,
            NodePhase::Suspended,
        ] {
            assert!(!phase.is_terminal());
        }
    }
}
