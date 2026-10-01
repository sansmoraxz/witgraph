//! Island lifecycle as a typestate machine.
//!
//! An island (the nodes that share one Store) is the unit that runs,
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
//!   Stopped --rebuild--> Idle
//! ```
//!
//! What an island still has to run is kept apart from its phase, in an
//! [`OwedQueue`]: input changes owe a generation whatever phase the island
//! is in, and the scheduler starts the island once it can.

use std::collections::{HashMap, VecDeque};
use std::task::{Context, Poll};

use futures::FutureExt;
use futures::future::BoxFuture;
use wasmtime::component::Val;
use witgraph_ir::PortName;

use crate::engine::{IslandStore, Outcome};
use crate::error::NodeFault;
use crate::node::NodePhase;

/// The external Value inputs of a generation, by member in plan order.
pub(crate) type StartInputs = Vec<HashMap<PortName, Val>>;

mod sealed {
    pub trait Sealed {}
}

/// An island lifecycle phase marker. Sealed: the set of phases is fixed.
pub(crate) trait Phase: sealed::Sealed {}

/// Built and waiting for its next generation. Owns the Store.
pub(crate) struct Idle {
    store: IslandStore,
    /// Whether a generation has finished since the island was built.
    ran: bool,
}

/// A generation is in flight. Owns the generation future, which owns the
/// Store until it resolves.
pub(crate) struct Running {
    future: BoxFuture<'static, Outcome>,
    /// The external inputs the generation started with, for snapshots.
    started_with: StartInputs,
}

/// Faulted, cancelled or shut down. Owns no Store; rebuilt before it runs
/// again.
pub(crate) struct Stopped {
    cause: StopCause,
}

/// Why an island stopped.
#[derive(Debug, Clone)]
pub(crate) enum StopCause {
    /// A generation (or a rebuild) faulted.
    Faulted(NodeFault),
    /// The host cancelled the island.
    Cancelled,
    /// The runtime graph was shut down.
    Shutdown,
}

impl sealed::Sealed for Idle {}
impl sealed::Sealed for Running {}
impl sealed::Sealed for Stopped {}
impl Phase for Idle {}
impl Phase for Running {}
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

impl Island<Idle> {
    /// A freshly built island that has not run yet.
    pub(crate) fn new(store: IslandStore) -> Self {
        Self {
            generation: 0,
            state: Idle { store, ran: false },
        }
    }

    /// Starts the next generation: `drive` turns the Store and the new
    /// generation number into the generation future.
    pub(crate) fn start(
        self,
        started_with: StartInputs,
        drive: impl FnOnce(IslandStore, u64) -> BoxFuture<'static, Outcome>,
    ) -> Island<Running> {
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

impl Island<Running> {
    /// Polls the generation. After it returns `Ready`, the island must be
    /// finished or stopped, not polled again.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Outcome> {
        self.state.future.poll_unpin(cx)
    }

    /// The generation finished and handed the Store back.
    pub(crate) fn finish(self, store: IslandStore) -> Island<Idle> {
        Island {
            generation: self.generation,
            state: Idle { store, ran: true },
        }
    }
}

impl Island<Stopped> {
    /// Re-instantiated into a fresh Store.
    pub(crate) fn rebuild(self, store: IslandStore) -> Island<Idle> {
        Island {
            generation: self.generation,
            state: Idle { store, ran: false },
        }
    }
}

/// An island in whichever phase it is currently in: the element type of
/// the scheduler's island table. Transitions dispatch to the typed
/// [`Island<S>`] for the current phase.
pub(crate) enum IslandState {
    /// See [`Idle`].
    Idle(Island<Idle>),
    /// See [`Running`].
    Running(Island<Running>),
    /// See [`Stopped`].
    Stopped(Island<Stopped>),
}

impl From<Island<Idle>> for IslandState {
    fn from(island: Island<Idle>) -> Self {
        Self::Idle(island)
    }
}

impl From<Island<Running>> for IslandState {
    fn from(island: Island<Running>) -> Self {
        Self::Running(island)
    }
}

impl From<Island<Stopped>> for IslandState {
    fn from(island: Island<Stopped>) -> Self {
        Self::Stopped(island)
    }
}

impl IslandState {
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

    /// Whether the island has no Store and must be rebuilt to run.
    pub(crate) fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped(_))
    }

    /// The external inputs of the generation in flight.
    pub(crate) fn started_with(&self) -> Option<&StartInputs> {
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
    pub(crate) fn node_phase(&self) -> NodePhase {
        match self {
            Self::Idle(island) if island.state.ran => NodePhase::Idle,
            Self::Idle(_) => NodePhase::Pending,
            Self::Running(_) => NodePhase::Running,
            Self::Stopped(island) => match island.state.cause {
                StopCause::Faulted(_) => NodePhase::Faulted,
                StopCause::Cancelled | StopCause::Shutdown => NodePhase::Cancelled,
            },
        }
    }

    /// The fault that stopped the island, if one did.
    pub(crate) fn fault(&self) -> Option<&NodeFault> {
        match self.stop_cause() {
            Some(StopCause::Faulted(fault)) => Some(fault),
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
    pub(crate) fn finish(self, store: IslandStore) -> Self {
        match self {
            Self::Running(island) => island.finish(store).into(),
            other => misuse(other, "Running"),
        }
    }

    /// A `Stopped` island was rebuilt into `store`.
    pub(crate) fn rebuild(self, store: IslandStore) -> Self {
        match self {
            Self::Stopped(island) => island.rebuild(store).into(),
            other => misuse(other, "Stopped"),
        }
    }
}

/// A transition was requested from a phase that does not allow it.
///
/// The scheduler always knows which phase an island is in, so this is a
/// bug: it trips a debug assertion, and in release builds the island is
/// left as it was.
pub(crate) fn misuse(state: IslandState, expected: &str) -> IslandState {
    debug_assert!(
        false,
        "island is {:?}, expected {expected}",
        state.node_phase()
    );
    state
}

/// One generation an island owes.
pub(crate) enum Owed {
    /// Run with the latched external inputs current when it starts.
    Latched,
    /// Replay a restored generation with exactly these inputs.
    Replay(StartInputs),
}

/// The generations an island owes, in order. Kept apart from the island's
/// phase: an input change owes a generation whether the island is idle,
/// running, or stopped.
#[derive(Default)]
pub(crate) struct OwedQueue(VecDeque<Owed>);

impl OwedQueue {
    /// Owes a run on the latched inputs. Coalesces with a trailing
    /// `Latched`: one re-run covers any number of changes before it starts.
    pub(crate) fn push_latched(&mut self) {
        if !matches!(self.0.back(), Some(Owed::Latched)) {
            self.0.push_back(Owed::Latched);
        }
    }

    /// Owes a replay of a restored generation.
    pub(crate) fn push_replay(&mut self, inputs: StartInputs) {
        self.0.push_back(Owed::Replay(inputs));
    }

    /// The next generation owed.
    pub(crate) fn front(&self) -> Option<&Owed> {
        self.0.front()
    }

    /// Takes the next generation owed.
    pub(crate) fn pop_front(&mut self) -> Option<Owed> {
        self.0.pop_front()
    }

    /// Forgets everything owed.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    /// Whether a run on the latched inputs is owed.
    pub(crate) fn has_latched(&self) -> bool {
        self.0.iter().any(|owed| matches!(owed, Owed::Latched))
    }

    /// The inputs of the first replay owed.
    pub(crate) fn replay(&self) -> Option<&StartInputs> {
        self.0.iter().find_map(|owed| match owed {
            Owed::Replay(inputs) => Some(inputs),
            Owed::Latched => None,
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latched_pushes_coalesce() {
        let mut owed = OwedQueue::default();
        owed.push_latched();
        owed.push_latched();
        owed.push_latched();
        assert_eq!(owed.len(), 1);
        assert!(owed.has_latched());
    }

    #[test]
    fn a_latched_after_a_replay_is_kept_and_then_coalesces() {
        let mut owed = OwedQueue::default();
        owed.push_replay(vec![HashMap::new()]);
        owed.push_latched();
        owed.push_latched();
        assert_eq!(owed.len(), 2);
        assert!(owed.replay().is_some());
        assert!(matches!(owed.pop_front(), Some(Owed::Replay(_))));
        assert!(matches!(owed.front(), Some(Owed::Latched)));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut owed = OwedQueue::default();
        owed.push_replay(Vec::new());
        owed.push_latched();
        owed.clear();
        assert!(owed.front().is_none());
        assert!(!owed.has_latched());
    }

    #[test]
    fn stopped_phases_project_onto_node_phases() {
        let faulted: IslandState = Island {
            generation: 3,
            state: Stopped {
                cause: StopCause::Faulted(NodeFault::FuelExhausted),
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
        assert!(shut.is_stopped());
    }
}
