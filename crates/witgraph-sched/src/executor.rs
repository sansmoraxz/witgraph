//! The seam between the scheduler and whatever runs the nodes.
//!
//! The scheduler decides *when* an island runs a generation; an
//! [`Executor`] runs it. It owns everything specific to an engine: how a
//! node is instantiated, how its `run` is called, and what a value is. The
//! scheduler handles values only through [`PortValue`] and the executor's
//! per-port operations.
//!
//! Inside an island the order of the members' `run` calls is the same for
//! every engine, so it lives here too: [`drive`] calls the members through
//! a [`NodeCaller`], in dependency order, and routes each output to the
//! members that read it.

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use futures::stream::{FuturesUnordered, StreamExt};
use witgraph_ir::{NodeId, PortDirection, PortKind, PortName, PortRef};

use crate::error::NodeFault;
use crate::plan::IslandPlan;

/// `Send` on native targets, nothing on `wasm32`: there a generation is
/// driven by JavaScript promises, which never leave their thread.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send` on native targets, nothing on `wasm32`: there a generation is
/// driven by JavaScript promises, which never leave their thread.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets, nothing on `wasm32` (see [`MaybeSend`]).
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}

/// `Sync` on native targets, nothing on `wasm32` (see [`MaybeSend`]).
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

/// The future of one generation: boxed, and `Send` on native targets.
#[cfg(not(target_arch = "wasm32"))]
pub type GenerationFuture<L> = futures::future::BoxFuture<'static, Outcome<L>>;

/// The future of one generation: boxed, and `Send` on native targets.
#[cfg(target_arch = "wasm32")]
pub type GenerationFuture<L> = futures::future::LocalBoxFuture<'static, Outcome<L>>;

/// A value a port carries, as the scheduler needs it: cloned to fan out,
/// compared to decide whether an input changed. Implemented for every type
/// that qualifies; what an option holds is the executor's to say
/// ([`Executor::option_payload`]).
pub trait PortValue: Clone + PartialEq + MaybeSend + MaybeSync + 'static {}
impl<T: Clone + PartialEq + MaybeSend + MaybeSync + 'static> PortValue for T {}

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
    pub events: UnboundedSender<IslandEvent<V>>,
}

impl<V> Generation<V> {
    /// Reports `kind` for this generation.
    pub fn send(&self, kind: IslandEventKind<V>) {
        send(&self.events, self.plan.index, self.number, kind);
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

/// Reports for one generation, as [`Generation::send`] does.
pub struct Reporter<V> {
    events: UnboundedSender<IslandEvent<V>>,
    island: usize,
    generation: u64,
}

impl<V> Reporter<V> {
    /// Reports `kind` for the generation.
    pub fn send(&self, kind: IslandEventKind<V>) {
        send(&self.events, self.island, self.generation, kind);
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

fn send<V>(
    events: &UnboundedSender<IslandEvent<V>>,
    island: usize,
    generation: u64,
    kind: IslandEventKind<V>,
) {
    // The receiver lives as long as the scheduler; a send can only fail
    // while it is being dropped.
    let _ = events.unbounded_send(IslandEvent {
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

/// What runs the nodes of a graph for a [`Scheduler`](crate::Scheduler).
pub trait Executor: 'static {
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
    fn check_input(&self, ty: &Self::Type, value: Self::Value) -> Result<Self::Value, String>;

    /// Parses WAVE text as a value of type `ty` (a Value port's), in the
    /// same form as [`check_input`](Self::check_input) returns. Record
    /// fields, cases and flags the type does not have are errors.
    fn parse_wave(&self, ty: &Self::Type, text: &str) -> Result<Self::Value, String>;

    /// A value of a Value port as WAVE text.
    fn to_wave(&self, value: &Self::Value) -> String;
}

/// How [`drive`] calls the members of one island.
pub trait NodeCaller<V: Clone>: MaybeSync {
    /// What a failed call reports.
    type Error;

    /// Calls member `member`'s `run`. `args` holds one value per field of
    /// its `inputs` record, in declaration order (`None` for an absent
    /// one); the result is its outputs, by field name.
    fn call(
        &self,
        member: usize,
        args: Vec<Option<V>>,
    ) -> impl Future<Output = Result<Vec<(String, V)>, Self::Error>> + MaybeSend;

    /// Drops a stream or future output nothing reads, so the guest's
    /// writes fail instead of blocking.
    fn close(&self, value: V) -> Result<(), Self::Error>;

    /// The error for a member whose required input `field` has no value
    /// when its `run` is due: [`drive`] reports it instead of calling.
    fn missing_input(&self, member: usize, field: &PortName) -> Self::Error;
}

/// The message for a member due to run without a value for its required
/// input `field`: what every executor reports, [`drive`] having checked.
pub fn missing_input_message(plan: &IslandPlan, member: usize, field: &PortName) -> String {
    let node = plan.members.get(member).map_or("?", |m| m.node.as_str());
    format!("no value for required input `{node}.{field}`")
}

/// What a connection that unwraps an option delivers for a value
/// ([`Executor::option_payload`], for the values [`drive`] moves).
pub type OptionPayloadFn<V> = for<'a> fn(&'a V) -> OptionPayload<'a, V>;

/// Calls every member's `run` in dependency order, concurrently, and
/// routes outputs. Returns once every call has returned, or at the first
/// failure (a failed call, a missing required input, or a failed
/// [`close`](NodeCaller::close)): the calls still in flight are then
/// dropped, and an executor whose calls run elsewhere (not inside the
/// dropped futures) must treat them as abandoned.
///
/// - A member's `run` is called once every in-island producer it reads
///   from has returned. Its arguments are those producers' fresh outputs
///   plus `args`, the host-supplied external Value inputs.
/// - A Value output is cloned to each in-island consumer; a stream or
///   future output is moved to its one consumer, or closed when it has
///   none.
/// - Each start and return is reported through `send`; a return carries
///   the member's Value outputs.
/// - A connection that unwraps an option delivers what `option_payload`
///   says.
pub async fn drive<V, C>(
    caller: &C,
    plan: &IslandPlan,
    mut args: Vec<Vec<Option<V>>>,
    option_payload: OptionPayloadFn<V>,
    send: &(impl Fn(IslandEventKind<V>) + MaybeSync),
) -> Result<(), C::Error>
where
    V: Clone + MaybeSend + MaybeSync + 'static,
    C: NodeCaller<V>,
{
    let mut waiting: Vec<usize> = plan.members.iter().map(|m| m.deps.len()).collect();
    let mut calls = FuturesUnordered::new();
    let call = |member: usize, args: Vec<Option<V>>| async move {
        (member, caller.call(member, args).await)
    };
    // A member is reported started only once its `run` can be called: every
    // required input has a value.
    let ready = |member: usize, args: &[Option<V>]| -> Result<(), C::Error> {
        let fields = plan.members[member].inputs.iter().flatten();
        match fields
            .zip(args)
            .find(|(field, arg)| !field.optional && arg.is_none())
        {
            Some((field, _)) => Err(caller.missing_input(member, &field.name)),
            None => Ok(()),
        }
    };
    for (i, remaining) in waiting.iter().enumerate() {
        if *remaining == 0 {
            ready(i, &args[i])?;
            send(IslandEventKind::RunStarted { member: i });
            calls.push(call(i, std::mem::take(&mut args[i])));
        }
    }

    while let Some((i, results)) = calls.next().await {
        let member = &plan.members[i];
        let mut values = Vec::new();
        for (name, val) in results? {
            let port = PortName::from(name);
            let Some(output) = member.outputs.get(&port) else {
                continue;
            };
            match output.kind {
                PortKind::Value => {
                    for consumer in &output.consumers {
                        let delivered = if consumer.unwrap_option {
                            option_payload(&val).delivered(&val)
                        } else {
                            Some(val.clone())
                        };
                        args[consumer.member][consumer.field] = delivered;
                    }
                    values.push((port, val));
                }
                PortKind::Stream | PortKind::Future => match output.consumers.first() {
                    Some(consumer) => args[consumer.member][consumer.field] = Some(val),
                    None => caller.close(val)?,
                },
            }
        }
        send(IslandEventKind::RunReturned { member: i, values });
        for &k in &member.dependents {
            waiting[k] -= 1;
            if waiting[k] == 0 {
                ready(k, &args[k])?;
                send(IslandEventKind::RunStarted { member: k });
                calls.push(call(k, std::mem::take(&mut args[k])));
            }
        }
    }
    Ok(())
}
