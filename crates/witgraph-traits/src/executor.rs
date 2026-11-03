//! The seam between the scheduler and whatever runs the nodes.
//!
//! The scheduler decides *when* an island runs a generation; an
//! [`Executor`] runs it. It owns everything specific to an engine: how a
//! node is instantiated, how its `run` is called, and what a value is. The
//! scheduler handles values only through [`PortValue`] and the executor's
//! per-port operations.
//!

use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

use witgraph_ir::{FeedbackIdx, InputIdx, NodeId, OutputIdx, PortDirection, PortName, PortRef};

use crate::error::NodeFault;
use crate::plan::IslandPlan;

/// `Send` on native targets, nothing on WebAssembly: there a generation is
/// driven by JavaScript promises, which never leave their thread.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send` on native targets, nothing on WebAssembly: there a generation is
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

/// The future of one generation: boxed, and `Send` on native targets.
#[cfg(not(target_family = "wasm"))]
pub type GenerationFuture<L> = futures::future::BoxFuture<'static, Outcome<L>>;

/// The future of one generation: boxed, and `Send` on native targets.
#[cfg(target_family = "wasm")]
pub type GenerationFuture<L> = futures::future::LocalBoxFuture<'static, Outcome<L>>;

/// A value a port carries, as the scheduler needs it: cloned to fan out,
/// compared to decide whether an input changed. Implemented for every type
/// that qualifies; what an option holds is the executor's to say
/// ([`Executor::option_payload`]).
pub trait PortValue: Clone + PartialEq + MaybeSend + MaybeSync + 'static {}
impl<T: Clone + PartialEq + MaybeSend + MaybeSync + 'static> PortValue for T {}

/// A value in the one form a guest produces, so that equal values compare
/// equal: value equality decides whether an input changed. The scheduler's
/// write path takes nothing else.
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

impl<V: Clone> OptionPayload<'_, V> {
    /// What the connection delivers for `value`, the value this was made
    /// from.
    pub fn delivered(self, value: &V) -> Option<V> {
        match self {
            Self::NotOption => Some(value.clone()),
            Self::Some(payload) => Some(payload.into_owned()),
            Self::None => None,
        }
    }
}

/// How a generation ended: the live island back, or the fault that killed
/// it (with the node that caused it, when that is known).
pub type Outcome<L> = Result<L, (NodeFault, Option<NodeId>)>;

/// One generation to run.
pub struct Generation<V> {
    /// The island's static wiring.
    pub plan: Arc<IslandPlan>,
    /// Per member, per field of its `inputs` record, the host-supplied
    /// Value input (`None` for an absent one, and for fields members of
    /// the island write).
    pub external: Vec<Vec<Option<V>>>,
    /// The island's generation number.
    pub number: u64,
    /// Whether [`IslandEventKind::StreamItems`] is wanted.
    pub stream_items: bool,
    /// Where the generation reports its [`IslandEvent`]s.
    pub events: Arc<dyn EventSink<V>>,
}

impl<V> Generation<V> {
    /// Reports `kind` for this generation.
    pub fn send(&self, kind: IslandEventKind<V>) {
        send(&*self.events, self.plan.index, self.number, kind);
    }

    /// What reports for this generation from work that cannot borrow it:
    /// a task that outlives the call that started it, say.
    pub fn reporter(&self) -> Reporter<V> {
        Reporter {
            events: self.events.clone(),
            island: self.plan.index,
            generation: self.number,
        }
    }
}

/// Takes what a generation reports.
pub trait EventSink<V>: MaybeSend + MaybeSync {
    /// Takes one report. A return's values are latched by the time this
    /// returns, so what reads them next sees them.
    fn send(&self, event: IslandEvent<V>);
}

/// Reports for one generation, as [`Generation::send`] does.
pub struct Reporter<V> {
    events: Arc<dyn EventSink<V>>,
    island: usize,
    generation: u64,
}

impl<V> Reporter<V> {
    /// Reports `kind` for the generation.
    pub fn send(&self, kind: IslandEventKind<V>) {
        send(&*self.events, self.island, self.generation, kind);
    }
}

impl<V> Clone for Reporter<V> {
    fn clone(&self) -> Self {
        Self {
            events: self.events.clone(),
            island: self.island,
            generation: self.generation,
        }
    }
}

fn send<V>(events: &dyn EventSink<V>, island: usize, generation: u64, kind: IslandEventKind<V>) {
    events.send(IslandEvent {
        island,
        generation,
        kind,
    });
}

/// A report from an in-flight generation.
#[derive(Debug)]
pub struct IslandEvent<V> {
    /// The island's index.
    pub island: usize,
    /// The generation reporting.
    pub generation: u64,
    /// What happened.
    pub kind: IslandEventKind<V>,
}

/// What happened in a generation, to the member at `member` in the island
/// plan's order.
#[derive(Debug)]
pub enum IslandEventKind<V> {
    /// The member's `run` was called.
    RunStarted {
        /// The member.
        member: usize,
    },
    /// The member's `run` returned.
    RunReturned {
        /// The member.
        member: usize,
        /// Its Value outputs, by port.
        values: Vec<(PortName, V)>,
    },
    /// The stopped island was rebuilt; its generation starts now.
    Rebuilt,
    /// Items of the member's stream output passed through the executor on
    /// their way to the stream's reader. Sent only when
    /// [`Generation::stream_items`] asks for it, by executors that move
    /// items themselves, possibly before the member's
    /// [`RunReturned`](Self::RunReturned): items pass as soon as the
    /// stream exists.
    StreamItems {
        /// The member whose output the stream is.
        member: usize,
        /// The output port.
        port: PortName,
        /// How many items passed.
        count: usize,
    },
}

impl<V> IslandEventKind<V> {
    /// The same event with its Value outputs mapped by `f`, keeping those
    /// it maps to `Some`.
    pub fn filter_map_values<W>(self, mut f: impl FnMut(V) -> Option<W>) -> IslandEventKind<W> {
        match self {
            Self::RunStarted { member } => IslandEventKind::RunStarted { member },
            Self::RunReturned { member, values } => IslandEventKind::RunReturned {
                member,
                values: values
                    .into_iter()
                    .filter_map(|(port, value)| Some((port, f(value)?)))
                    .collect(),
            },
            Self::Rebuilt => IslandEventKind::Rebuilt,
            Self::StreamItems {
                member,
                port,
                count,
            } => IslandEventKind::StreamItems {
                member,
                port,
                count,
            },
        }
    }
}

/// What a `run` returning changed, as the scheduler needs it: which ports,
/// never their values. So the scheduler can tell when to run what, wherever
/// the values are held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Returned {
    /// The outputs the `run` wrote.
    pub outputs_changed: Vec<OutputIdx>,
    /// The inputs that now hold a different value, and whose node owes a run.
    pub inputs_changed: Vec<InputIdx>,
    /// The inputs written inside the generation: they hold a value now, and
    /// their node, part of this generation, owes nothing.
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

/// A live island, opaque to the scheduler: what its data plane handed back
/// from the last generation.
pub trait LiveIsland: Any + MaybeSend {}
impl<T: Any + MaybeSend> LiveIsland for T {}

/// What the scheduler is told when a generation is due.
pub struct StartGeneration {
    /// The island's static wiring.
    pub plan: Arc<IslandPlan>,
    /// The island's generation number.
    pub number: u64,
    /// Whether [`IslandEventKind::StreamItems`] is wanted.
    pub stream_items: bool,
    /// Whether this replays a restored generation, on the inputs it started
    /// with, rather than running on the latched inputs.
    pub replay: bool,
}

/// What holds a graph's values and runs its nodes, as the scheduler sees it.
/// The scheduler decides when; through this it never sees a value.
pub trait DataPlane: MaybeSend + 'static {
    /// Runs one generation in a live island.
    fn run(
        &self,
        island: Box<dyn LiveIsland>,
        start: StartGeneration,
    ) -> GenerationFuture<Box<dyn LiveIsland>>;

    /// Rebuilds a stopped island, then runs one generation in it.
    fn rebuild_and_run(&self, start: StartGeneration) -> GenerationFuture<Box<dyn LiveIsland>>;

    /// An island's generation ended, however it ended.
    fn generation_ended(&self, island: usize);

    /// Writes the buffered value of a feedback connection to its input.
    fn latch_feedback(&self, connection: FeedbackIdx) -> Latched;

    /// Forgets the buffered value of a feedback connection.
    fn drop_feedback(&self, connection: FeedbackIdx);
}

/// What runs the nodes of a graph for a scheduler.
pub trait Executor: MaybeSend + 'static {
    /// The values Value ports carry.
    type Value: PortValue;

    /// The types of Value ports.
    type Type: 'static;

    /// A live island: what holds its members' guest state between
    /// generations (a wasmtime Store, say). An idle island owns one; a
    /// running generation owns it until it finishes; a stopped island has
    /// none, and is rebuilt.
    type Island: MaybeSend + 'static;

    /// Runs one generation in a live island, handing the island back when
    /// every member's `run` has returned and no guest work is left.
    fn run(
        &self,
        island: Self::Island,
        generation: Generation<Self::Value>,
    ) -> GenerationFuture<Self::Island>;

    /// Rebuilds island `generation.plan.index` from nothing (fresh guest
    /// state), reports [`IslandEventKind::Rebuilt`], then runs the
    /// generation in it. A failed rebuild is the fault
    /// [`NodeFault::Restart`].
    fn rebuild_and_run(
        &self,
        generation: Generation<Self::Value>,
    ) -> GenerationFuture<Self::Island>;

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

/// What a connection that unwraps an option delivers for a value
/// ([`Executor::option_payload`]).
pub type OptionPayloadFn<V> = for<'a> fn(&'a V) -> OptionPayload<'a, V>;
