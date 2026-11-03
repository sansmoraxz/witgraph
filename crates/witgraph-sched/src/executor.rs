//! Inside an island the order of the members' `run` calls is the same for
//! every engine, so it lives here: [`drive`] calls the members through a
//! [`NodeCaller`], in dependency order, and routes each output to the members
//! that read it. The seams themselves are in [`witgraph_traits::executor`].

use std::future::Future;

use futures::stream::{FuturesUnordered, StreamExt};
use witgraph_ir::{PortKind, PortName};
pub use witgraph_traits::executor::*;

use crate::plan::IslandPlan;

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
/// - A Value or handle output is cloned to each in-island consumer; a
///   stream or future output is moved to its one consumer, or closed when
///   it has none.
/// - Each start and return is reported through `send`; a return carries
///   the member's Value and handle outputs.
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
                PortKind::Value | PortKind::Handle => {
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
