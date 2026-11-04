//! What calls a node, for a plane that keeps its values in the host.

use std::borrow::Cow;
use std::sync::Arc;

use witgraph_ir::{NodeId, PortDirection, PortRef};
use witgraph_traits::{BoxFuture, MaybeSend, MaybeSync, NodeFault, Outcome};

use crate::plan::MemberPlan;

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
