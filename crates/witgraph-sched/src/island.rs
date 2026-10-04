//! Island lifecycle as a typestate machine.
//!
//! An island (the nodes that share one Store, on wasmtime) is the unit that runs,
//! faults and is cancelled, so it is the island that carries a lifecycle;
//! a node's phase is a projection of its island's (see
//! [`IslandState::node_phase`]). The lifecycle is modelled by [`Island<S>`],
//! where the marker type `S` is the phase. Each phase owns exactly the data
//! that is meaningful in it, and every transition consumes the island and
//! returns it in its next phase, so a state the lifecycle does not allow
//! cannot be represented:
//!
//! - [`Idle`] owns the Store, ready for the next generation.
//! - [`Running`] owns the generation future, which owns the Store; dropping
//!   the island drops the generation and the Store with it.
//! - [`Stopped`] owns no Store, only why it stopped.
//!
//! ```text
//!   Idle --start--> Running --finish--> Idle
//!   Idle | Running | Stopped --stop(cause)--> Stopped
//!   Stopped --start_rebuilt--> Running
//! ```
//!
//! What an island still has to run is kept apart from its phase, in
//! [`Owed`]: input changes owe a generation whatever phase the island is
//! in, and the scheduler starts the island once it can.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::FutureExt;
use witgraph_ir::{ConnectionId, NodeId};

use crate::error::NodeFault;
use crate::executor::{GenerationFuture, Outcome};
use crate::node::NodePhase;

/// The external Value inputs of a generation: per member in plan order,
/// per field of its `inputs` record (`None` for a field with no value, or
/// one a member of the island writes). Shared with the latched values they
/// were read from.
pub(crate) type StartInputs<V> = Vec<Vec<Option<Arc<V>>>>;

mod sealed {
    pub trait Sealed {}
}

/// An island lifecycle phase marker. Sealed: the set of phases is fixed.
pub(crate) trait Phase: sealed::Sealed {}

/// Built and waiting for its next generation. Owns the Store.
pub(crate) struct Idle<L> {
    store: L,
    /// Whether a generation has finished since the island was built.
    ran: bool,
}

/// A generation is in flight. Owns the generation future, which owns the
/// Store until it resolves.
pub(crate) struct Running<L, V> {
    future: GenerationFuture<L>,
    /// The external inputs the generation started with, for snapshots.
    started_with: StartInputs<V>,
}

/// Faulted, cancelled or shut down. Owns no Store; rebuilt before it runs
/// again.
pub(crate) struct Stopped {
    cause: StopCause,
}

/// Why an island stopped.
#[derive(Debug, Clone)]
pub(crate) enum StopCause {
    /// A generation (or a rebuild) faulted; the member that caused it,
    /// when it is known (see [`NodeState::culprit`](crate::NodeState::culprit)).
    Faulted(NodeFault, Option<NodeId>),
    /// The host cancelled the island.
    Cancelled,
    /// A snapshot was restored: the island's guest state was dropped, so
    /// none outlives the restore.
    Restored,
    /// The runtime graph was shut down.
    Shutdown,
}

impl<L> sealed::Sealed for Idle<L> {}
impl<L, V> sealed::Sealed for Running<L, V> {}
impl sealed::Sealed for Stopped {}
impl<L> Phase for Idle<L> {}
impl<L, V> Phase for Running<L, V> {}
impl Phase for Stopped {}

/// An island in lifecycle phase `S`.
pub(crate) struct Island<S> {
    /// The last generation started. Kept across every transition, so
    /// events from a dropped generation never match a later one.
    generation: u64,
    state: S,
}

impl<S: Phase> Island<S> {
    /// Stops the island. From `Running` this drops the generation future,
    /// and with it the Store; from `Stopped` it replaces the cause.
    pub(crate) fn stop(self, cause: StopCause) -> Island<Stopped> {
        Island {
            generation: self.generation,
            state: Stopped { cause },
        }
    }
}

impl<L> Island<Idle<L>> {
    /// A freshly built island that has not run yet.
    pub(crate) fn new(store: L) -> Self {
        Self {
            generation: 0,
            state: Idle { store, ran: false },
        }
    }

    /// Starts the next generation: `drive` turns the Store and the new
    /// generation number into the generation future.
    pub(crate) fn start<V>(
        self,
        started_with: StartInputs<V>,
        drive: impl FnOnce(L, u64) -> GenerationFuture<L>,
    ) -> Island<Running<L, V>> {
        let generation = self.generation + 1;
        Island {
            generation,
            state: Running {
                future: drive(self.state.store, generation),
                started_with,
            },
        }
    }
}

impl<L, V> Island<Running<L, V>> {
    /// Polls the generation. After it returns `Ready`, the island must be
    /// finished or stopped, not polled again.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Outcome<L>> {
        self.state.future.poll_unpin(cx)
    }

    /// The generation finished and handed the Store back.
    pub(crate) fn finish(self, store: L) -> Island<Idle<L>> {
        Island {
            generation: self.generation,
            state: Idle { store, ran: true },
        }
    }
}

impl Island<Stopped> {
    /// Starts the next generation from no Store: `drive` turns the new
    /// generation number into a future that rebuilds the island's Store and
    /// then runs the generation in it.
    pub(crate) fn start_rebuilt<L, V>(
        self,
        started_with: StartInputs<V>,
        drive: impl FnOnce(u64) -> GenerationFuture<L>,
    ) -> Island<Running<L, V>> {
        let generation = self.generation + 1;
        Island {
            generation,
            state: Running {
                future: drive(generation),
                started_with,
            },
        }
    }
}

/// An island in whichever phase it is currently in: the element type of
/// the scheduler's island table. Transitions dispatch to the typed
/// [`Island<S>`] for the current phase.
pub(crate) enum IslandState<L, V> {
    /// See [`Idle`].
    Idle(Island<Idle<L>>),
    /// See [`Running`].
    Running(Island<Running<L, V>>),
    /// See [`Stopped`].
    Stopped(Island<Stopped>),
}

impl<L, V> From<Island<Idle<L>>> for IslandState<L, V> {
    fn from(island: Island<Idle<L>>) -> Self {
        Self::Idle(island)
    }
}

impl<L, V> From<Island<Running<L, V>>> for IslandState<L, V> {
    fn from(island: Island<Running<L, V>>) -> Self {
        Self::Running(island)
    }
}

impl<L, V> From<Island<Stopped>> for IslandState<L, V> {
    fn from(island: Island<Stopped>) -> Self {
        Self::Stopped(island)
    }
}

impl<L, V> IslandState<L, V> {
    /// A stand-in for the moment a transition holds the real state.
    pub(crate) fn placeholder() -> Self {
        Self::Stopped(Island {
            generation: 0,
            state: Stopped {
                cause: StopCause::Shutdown,
            },
        })
    }

    /// The generation in flight, if one is.
    pub(crate) fn running_generation(&self) -> Option<u64> {
        match self {
            Self::Running(island) => Some(island.generation),
            _ => None,
        }
    }

    /// Whether a generation is in flight.
    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    /// The external inputs of the generation in flight.
    pub(crate) fn started_with(&self) -> Option<&StartInputs<V>> {
        match self {
            Self::Running(island) => Some(&island.state.started_with),
            _ => None,
        }
    }

    /// Why the island stopped, if it did.
    pub(crate) fn stop_cause(&self) -> Option<&StopCause> {
        match self {
            Self::Stopped(island) => Some(&island.state.cause),
            _ => None,
        }
    }

    /// The phase every member node is in:
    ///
    /// | island                    | node phase  |
    /// |---------------------------|-------------|
    /// | `Idle`, never ran         | `Pending`   |
    /// | `Idle`, ran               | `Idle`      |
    /// | `Running`                 | `Running`   |
    /// | `Stopped(Faulted)`        | `Faulted`   |
    /// | `Stopped(Cancelled)`, `Stopped(Shutdown)` | `Cancelled` |
    /// | `Stopped(Restored)`       | `Pending`   |
    pub(crate) fn node_phase(&self) -> NodePhase {
        match self {
            Self::Idle(island) if island.state.ran => NodePhase::Idle,
            Self::Idle(_) => NodePhase::Pending,
            Self::Running(_) => NodePhase::Running,
            Self::Stopped(island) => match island.state.cause {
                StopCause::Faulted(..) => NodePhase::Faulted,
                StopCause::Cancelled | StopCause::Shutdown => NodePhase::Cancelled,
                StopCause::Restored => NodePhase::Pending,
            },
        }
    }

    /// The member that caused the fault that stopped the island, if known.
    pub(crate) fn culprit(&self) -> Option<&NodeId> {
        match self.stop_cause() {
            Some(StopCause::Faulted(_, culprit)) => culprit.as_ref(),
            _ => None,
        }
    }

    /// The fault that stopped the island, if one did.
    pub(crate) fn fault(&self) -> Option<&NodeFault> {
        match self.stop_cause() {
            Some(StopCause::Faulted(fault, _)) => Some(fault),
            _ => None,
        }
    }

    /// Stops the island from any phase.
    pub(crate) fn stop(self, cause: StopCause) -> Self {
        match self {
            Self::Idle(island) => island.stop(cause).into(),
            Self::Running(island) => island.stop(cause).into(),
            Self::Stopped(island) => island.stop(cause).into(),
        }
    }

    /// A `Running` island's generation finished with `store`.
    pub(crate) fn finish(self, store: L) -> Self {
        match self {
            Self::Running(island) => island.finish(store).into(),
            other => misuse(other, "Running"),
        }
    }
}

/// A transition was requested from a phase that does not allow it.
///
/// The scheduler always knows which phase an island is in, so this is a
/// bug: it trips a debug assertion, and in release builds the island is
/// left as it was.
pub(crate) fn misuse<L, V>(state: IslandState<L, V>, expected: &str) -> IslandState<L, V> {
    debug_assert!(
        false,
        "island is {:?}, expected {expected}",
        state.node_phase()
    );
    state
}

/// A restored generation to replay.
pub(crate) struct Replay<V> {
    /// The external inputs it started with.
    pub(crate) inputs: StartInputs<V>,
    /// Feedback connections whose target the host wrote after the original
    /// generation started, or after the restore: what it feeds back there
    /// is stale.
    pub(crate) stale: BTreeSet<ConnectionId>,
}

/// One generation to start.
pub(crate) enum Work<V> {
    /// Run with the latched external inputs current when it starts.
    Latched,
    /// Replay a restored generation.
    Replay(Replay<V>),
}

/// What an island owes: a replay of a restored generation, which runs
/// first, and a run on the latched inputs. Kept apart from the island's
/// phase: an input change owes a run whether the island is idle, running,
/// or stopped, and any number of changes before the next start owe one.
pub(crate) struct Owed<V> {
    replay: Option<Replay<V>>,
    latched: bool,
}

impl<V> Default for Owed<V> {
    fn default() -> Self {
        Self {
            replay: None,
            latched: false,
        }
    }
}

impl<V> Owed<V> {
    /// Owes a run on the latched inputs.
    pub(crate) fn push_latched(&mut self) {
        self.latched = true;
    }

    /// Owes a replay of a restored generation, before any run.
    pub(crate) fn set_replay(&mut self, replay: Replay<V>) {
        self.replay = Some(replay);
    }

    /// Whether nothing is owed.
    pub(crate) fn is_empty(&self) -> bool {
        self.replay.is_none() && !self.latched
    }

    /// Whether the next generation owed is a replay.
    pub(crate) fn next_is_replay(&self) -> bool {
        self.replay.is_some()
    }

    /// Takes the next generation owed.
    pub(crate) fn take_next(&mut self) -> Option<Work<V>> {
        if let Some(replay) = self.replay.take() {
            return Some(Work::Replay(replay));
        }
        std::mem::take(&mut self.latched).then_some(Work::Latched)
    }

    /// Forgets everything owed.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether a run on the latched inputs is owed.
    pub(crate) fn has_latched(&self) -> bool {
        self.latched
    }

    /// The replay owed, if one is.
    pub(crate) fn replay(&self) -> Option<&Replay<V>> {
        self.replay.as_ref()
    }

    /// The replay owed, if one is, to mark more of its feedback stale.
    pub(crate) fn replay_mut(&mut self) -> Option<&mut Replay<V>> {
        self.replay.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay() -> Replay<u32> {
        Replay {
            inputs: vec![Vec::new()],
            stale: BTreeSet::new(),
        }
    }

    #[test]
    fn latched_runs_coalesce() {
        let mut owed = Owed::<u32>::default();
        owed.push_latched();
        owed.push_latched();
        assert!(owed.has_latched());
        assert!(matches!(owed.take_next(), Some(Work::Latched)));
        assert!(owed.take_next().is_none());
        assert!(owed.is_empty());
    }

    #[test]
    fn a_replay_runs_before_the_latched_run() {
        let mut owed = Owed::default();
        owed.push_latched();
        owed.set_replay(replay());
        assert!(owed.next_is_replay());
        assert!(owed.replay().is_some());
        assert!(matches!(owed.take_next(), Some(Work::Replay(_))));
        assert!(matches!(owed.take_next(), Some(Work::Latched)));
        assert!(owed.take_next().is_none());
    }

    #[test]
    fn clear_forgets_everything() {
        let mut owed = Owed::default();
        owed.set_replay(replay());
        owed.push_latched();
        owed.clear();
        assert!(owed.is_empty());
        assert!(!owed.has_latched());
    }

    #[test]
    fn stopped_phases_project_onto_node_phases() {
        let faulted: IslandState<(), u32> = Island {
            generation: 3,
            state: Stopped {
                cause: StopCause::Faulted(NodeFault::FuelExhausted, None),
            },
        }
        .into();
        assert_eq!(faulted.node_phase(), NodePhase::Faulted);
        assert!(matches!(faulted.fault(), Some(NodeFault::FuelExhausted)));

        let cancelled = faulted.stop(StopCause::Cancelled);
        assert_eq!(cancelled.node_phase(), NodePhase::Cancelled);
        assert!(
            cancelled.fault().is_none(),
            "the new cause replaces the fault"
        );
        assert_eq!(cancelled.running_generation(), None);

        let shut = cancelled.stop(StopCause::Shutdown);
        assert_eq!(shut.node_phase(), NodePhase::Cancelled);
        assert!(matches!(shut, IslandState::Stopped(_)));
    }
}
