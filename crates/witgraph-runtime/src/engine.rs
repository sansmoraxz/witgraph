//! Wasmtime plumbing: the engine, island Stores, and the generation driver.
//!
//! # Islands
//!
//! An island ([`CompiledGraph::islands`](witgraph_ir::CompiledGraph::islands))
//! is a set of nodes joined by stream/future connections. All of its nodes
//! are instantiated in one wasmtime [`Store`], because a component-model
//! stream or future handle belongs to one Store and the host cannot move
//! items of an arbitrary payload type between Stores. A Value-only node is
//! an island of one.
//!
//! # Generations
//!
//! `run_generation` drives one generation of an island inside
//! [`Store::run_concurrent`]. Invariants:
//!
//! - A member's `run` is called once every in-island producer it reads
//!   from (over a non-feedback connection, of any kind) has returned. Its
//!   arguments are those producers' fresh outputs plus the host-supplied
//!   external Value inputs. Stream/future handles are passed straight from
//!   the producer's results into the consumer's arguments; the host never
//!   touches their items.
//! - Calls are concurrent: a member's call starts as soon as its own
//!   dependencies have returned, independently of its siblings.
//! - A stream/future output with no in-island consumer is closed as soon as
//!   its `run` returns, so the guest's writes fail instead of blocking.
//! - Each `run` start and return is reported as an island event; a
//!   return carries the member's Value outputs, so the host can latch them
//!   and wake downstream islands while this generation is still going.
//! - The generation finishes when every `run` has returned **and** no guest
//!   task is left in the Store
//!   ([`Accessor::poll_no_interesting_tasks`]): work a guest spawned after
//!   returning, such as a stream writer, belongs to the generation.
//! - Any error, a trap in a spawned task included, faults the whole island:
//!   a trap poisons its Store.
//!
//! # Sandboxing
//!
//! Every island Store meters fuel. `fuel_async_yield_interval` makes a busy
//! island yield to the executor every `yield_interval` units, so other
//! islands keep running; there is no interleaving *within* one Store. The
//! Store's fuel is reset to the per-run budget before every `run` call (the
//! budget is shared by everything executing in the island, spawned tasks
//! included). An island that burns the budget before its next `run` starts
//! traps with [`NodeFault::FuelExhausted`]; that includes an endless
//! streaming generation once it has consumed the budget, so endless islands
//! need an unlimited (`None`) or suitably large budget.

use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use futures::stream::{FuturesUnordered, StreamExt};
use wasmtime::component::{Accessor, Component, Func, Linker, Type, Val};
use wasmtime::{AsContextMut, Engine, Store, StoreContextMut};
use witgraph_ir::{NodeId, PortKind, PortName};

use crate::error::NodeFault;

/// Host state carried by every island Store.
///
/// Embedders see this type only as the data parameter of the
/// [`Linker`] handed to
/// [`RuntimeGraph::load_with_linker`](crate::RuntimeGraph::load_with_linker)'s
/// callback; it has no public API.
#[derive(Debug, Default)]
pub struct HostState {
    /// The first `fatal` call in this Store: the calling node and message.
    fatal: Option<(NodeId, String)>,
}

/// Creates the engine every island shares.
pub(crate) fn new_engine() -> wasmtime::Result<Engine> {
    let mut config = wasmtime::Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .concurrency_support(true)
        .consume_fuel(true);
    Engine::new(&config)
}

/// The fully qualified name of the built-in host interface.
const HOST_INTERFACE: &str = "witgraph:runtime/host@0.1.0";

/// A linker for one node: the built-in `witgraph:runtime/host` (whose
/// `fatal` knows which node called it) plus the embedder's capabilities.
pub(crate) fn node_linker(
    engine: &Engine,
    node: &NodeId,
    configure: &dyn Fn(&mut Linker<HostState>) -> wasmtime::Result<()>,
) -> wasmtime::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    let caller = node.clone();
    linker.instance(HOST_INTERFACE)?.func_wrap(
        "fatal",
        move |mut store: StoreContextMut<'_, HostState>, (message,): (String,)| {
            let state = store.data_mut();
            if state.fatal.is_none() {
                state.fatal = Some((caller.clone(), message.clone()));
            }
            Err::<(), _>(wasmtime::format_err!("fatal: {message}"))
        },
    )?;
    configure(&mut linker)?;
    Ok(linker)
}

/// Fuel settings applied to every island Store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FuelPolicy {
    /// Fuel between cooperative yields to the executor.
    pub(crate) yield_interval: Option<u64>,
    /// Fuel an island may burn between two `run` starts; `None` is
    /// effectively unlimited.
    pub(crate) per_run: Option<u64>,
}

impl FuelPolicy {
    /// The fuel level set at Store creation and before every `run` call.
    pub(crate) fn budget(self) -> u64 {
        self.per_run.unwrap_or(u64::MAX / 2)
    }
}

/// Where an input field of a member's `inputs` record comes from in a
/// generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSource {
    /// Supplied by the host from its latched input values: unconnected
    /// ports, feedback connections, and connections from other islands.
    External,
    /// Produced by another member of the island earlier in the same
    /// generation (routed through [`OutputField::consumers`]).
    Member,
}

/// One field of a member's `inputs` record, in declaration order.
#[derive(Debug, Clone)]
pub(crate) struct InputField {
    pub(crate) name: PortName,
    pub(crate) kind: PortKind,
    /// Declared `option<T>`: the host wraps the value (or passes `none`).
    pub(crate) optional: bool,
    pub(crate) source: InputSource,
}

/// One field of a member's `outputs` record.
#[derive(Debug, Clone)]
pub(crate) struct OutputField {
    pub(crate) kind: PortKind,
    /// In-island consumers over non-feedback connections: (member, input
    /// field). At most one for a stream or future.
    pub(crate) consumers: Vec<(usize, PortName)>,
}

/// Static wiring of one island member, computed at load.
#[derive(Debug, Clone)]
pub(crate) struct MemberPlan {
    pub(crate) node: NodeId,
    /// `None` when `run` takes no `inputs` record.
    pub(crate) inputs: Option<Vec<InputField>>,
    /// Whether `run` returns an `outputs` record.
    pub(crate) has_result: bool,
    pub(crate) outputs: HashMap<PortName, OutputField>,
    /// Members whose `run` must return before this one is called.
    pub(crate) deps: Vec<usize>,
    /// Members that list this one in their `deps`.
    pub(crate) dependents: Vec<usize>,
}

/// Static wiring of one island, computed at load. Members are in
/// topological order.
#[derive(Debug, Clone)]
pub(crate) struct IslandPlan {
    pub(crate) index: usize,
    pub(crate) members: Vec<MemberPlan>,
}

/// A live island's wasmtime side: its Store and each member's `run` export.
/// The island's scheduling state is in [`crate::island`].
pub(crate) struct IslandStore {
    store: Store<HostState>,
    runs: Vec<Func>,
}

/// The shape of a member's `run` export as the component declares it.
pub(crate) struct RunSignature {
    /// The `inputs` record's fields in declaration order, with their
    /// component types; `None` when `run` takes no parameter.
    pub(crate) inputs: Option<Vec<(String, Type)>>,
    /// Whether `run` returns a result.
    pub(crate) has_result: bool,
    /// The `outputs` record's fields with their component types; empty
    /// when `run` returns nothing.
    pub(crate) outputs: Vec<(String, Type)>,
}

/// Instantiates every member of an island into a fresh Store.
///
/// `members` pairs each member's node with its compiled component and
/// linker, in the island plan's order.
pub(crate) async fn build_island(
    engine: &Engine,
    members: &[(&NodeId, &Component, &Linker<HostState>)],
    fuel: FuelPolicy,
) -> Result<(IslandStore, Vec<RunSignature>), (NodeId, String)> {
    let mut store = Store::new(engine, HostState::default());
    let setup = |store: &mut Store<HostState>| -> wasmtime::Result<()> {
        if fuel.yield_interval.is_some() {
            store.fuel_async_yield_interval(fuel.yield_interval)?;
        }
        store.set_fuel(fuel.budget())?;
        Ok(())
    };
    let first = members.first().map(|(node, _, _)| (*node).clone());
    setup(&mut store).map_err(|e| (first.unwrap_or_else(|| "?".into()), format!("{e:#}")))?;

    let mut runs = Vec::with_capacity(members.len());
    let mut signatures = Vec::with_capacity(members.len());
    for (node, component, linker) in members {
        let fail = |e: wasmtime::Error| ((*node).clone(), format!("{e:#}"));
        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(fail)?;
        let missing = |what: &str| ((*node).clone(), format!("component exports no `{what}`"));
        let node_export = instance
            .get_export_index(&mut store, None, "node")
            .ok_or_else(|| missing("node"))?;
        let run_export = instance
            .get_export_index(&mut store, Some(&node_export), "run")
            .ok_or_else(|| missing("node#run"))?;
        let run = instance
            .get_func(&mut store, run_export)
            .ok_or_else(|| missing("node#run function"))?;
        signatures.push(signature(&store, run));
        runs.push(run);
    }
    Ok((IslandStore { store, runs }, signatures))
}

fn signature(store: &Store<HostState>, run: Func) -> RunSignature {
    let ty = run.ty(store);
    let inputs = ty.params().next().map(|(_, param)| match param {
        Type::Record(record) => record
            .fields()
            .map(|field| (field.name.to_string(), field.ty))
            .collect(),
        _ => Vec::new(),
    });
    let outputs = match ty.results().next() {
        Some(Type::Record(record)) => record
            .fields()
            .map(|field| (field.name.to_string(), field.ty))
            .collect(),
        _ => Vec::new(),
    };
    RunSignature {
        inputs,
        has_result: ty.results().len() > 0,
        outputs,
    }
}

/// A report from an in-flight generation.
#[derive(Debug)]
pub(crate) struct IslandEvent {
    pub(crate) island: usize,
    pub(crate) generation: u64,
    pub(crate) kind: IslandEventKind,
}

#[derive(Debug)]
pub(crate) enum IslandEventKind {
    RunStarted {
        node: NodeId,
    },
    RunReturned {
        node: NodeId,
        /// The member's Value outputs, by port.
        values: Vec<(PortName, Val)>,
    },
}

/// How a generation ended: the island back, or the fault that killed it
/// (with the node that called `fatal`, if one did).
pub(crate) type Outcome = Result<IslandStore, (NodeFault, Option<NodeId>)>;

/// Runs one generation of `island`. `external` holds, per member, the
/// host-supplied Value inputs (missing optional inputs are simply absent).
pub(crate) async fn run_generation(
    mut island: IslandStore,
    plan: Arc<IslandPlan>,
    external: Vec<HashMap<PortName, Val>>,
    fuel: FuelPolicy,
    generation: u64,
    events: UnboundedSender<IslandEvent>,
) -> Outcome {
    island.store.data_mut().fatal = None;
    let budget = fuel.budget();
    let runs = island.runs.clone();
    let send = |kind| {
        // The receiver lives as long as the runtime graph; a send can only
        // fail while the graph is being dropped.
        let _ = events.unbounded_send(IslandEvent {
            island: plan.index,
            generation,
            kind,
        });
    };
    let driven = island
        .store
        .run_concurrent(async |acc| -> wasmtime::Result<()> {
            drive(acc, &plan, &runs, external, budget, &send).await?;
            poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
            Ok(())
        })
        .await;
    match driven.and_then(|inner| inner) {
        Ok(()) => Ok(island),
        Err(error) => {
            let fatal = island.store.data_mut().fatal.take();
            Err(classify(&error, fatal))
        }
    }
}

/// Calls every member's `run` in dependency order, concurrently, and
/// routes outputs. Returns once every call has returned.
async fn drive(
    acc: &Accessor<HostState>,
    plan: &IslandPlan,
    runs: &[Func],
    mut args: Vec<HashMap<PortName, Val>>,
    budget: u64,
    send: &impl Fn(IslandEventKind),
) -> wasmtime::Result<()> {
    let mut waiting: Vec<usize> = plan.members.iter().map(|m| m.deps.len()).collect();
    let mut calls = FuturesUnordered::new();
    for (i, member) in plan.members.iter().enumerate() {
        if waiting[i] == 0 {
            let params = params(member, std::mem::take(&mut args[i]))?;
            send(IslandEventKind::RunStarted {
                node: member.node.clone(),
            });
            calls.push(call(acc, runs[i], params, member.has_result, budget, i));
        }
    }

    while let Some((i, results)) = calls.next().await {
        let results = results?;
        let member = &plan.members[i];
        let mut values = Vec::new();
        if let Some(Val::Record(fields)) = results.into_iter().next() {
            for (name, val) in fields {
                let port = PortName::from(name);
                let Some(output) = member.outputs.get(&port) else {
                    continue;
                };
                match output.kind {
                    PortKind::Value => {
                        for (consumer, input) in &output.consumers {
                            args[*consumer].insert(input.clone(), val.clone());
                        }
                        values.push((port, val));
                    }
                    PortKind::Stream | PortKind::Future => match output.consumers.first() {
                        Some((consumer, input)) => {
                            args[*consumer].insert(input.clone(), val);
                        }
                        None => close(acc, val)?,
                    },
                }
            }
        }
        send(IslandEventKind::RunReturned {
            node: member.node.clone(),
            values,
        });
        for &k in &member.dependents {
            waiting[k] -= 1;
            if waiting[k] == 0 {
                let target = &plan.members[k];
                let params = params(target, std::mem::take(&mut args[k]))?;
                send(IslandEventKind::RunStarted {
                    node: target.node.clone(),
                });
                calls.push(call(acc, runs[k], params, target.has_result, budget, k));
            }
        }
    }
    Ok(())
}

async fn call(
    acc: &Accessor<HostState>,
    run: Func,
    params: Vec<Val>,
    has_result: bool,
    budget: u64,
    index: usize,
) -> (usize, wasmtime::Result<Vec<Val>>) {
    if let Err(e) = acc.with(|mut access| access.as_context_mut().set_fuel(budget)) {
        return (index, Err(e));
    }
    let mut results = if has_result {
        vec![Val::Bool(false)]
    } else {
        Vec::new()
    };
    let outcome = run.call_concurrent(acc, &params, &mut results).await;
    (index, outcome.map(|()| results))
}

/// Builds `run`'s parameter list from a member's collected inputs.
fn params(member: &MemberPlan, mut args: HashMap<PortName, Val>) -> wasmtime::Result<Vec<Val>> {
    let Some(fields) = &member.inputs else {
        return Ok(Vec::new());
    };
    let mut record = Vec::with_capacity(fields.len());
    for field in fields {
        let value = args.remove(&field.name);
        let value = if field.optional {
            Val::Option(value.map(Box::new))
        } else {
            value.ok_or_else(|| {
                wasmtime::format_err!(
                    "no value for required input `{}.{}`",
                    member.node,
                    field.name
                )
            })?
        };
        record.push((field.name.to_string(), value));
    }
    Ok(vec![Val::Record(record)])
}

/// Drops the host's copy of an unconsumed stream or future handle so the
/// guest's writes fail instead of blocking.
fn close(acc: &Accessor<HostState>, val: Val) -> wasmtime::Result<()> {
    match val {
        Val::Stream(mut stream) => acc.with(|mut access| stream.close(&mut access)),
        Val::Future(mut future) => acc.with(|mut access| future.close(&mut access)),
        _ => Ok(()),
    }
}

fn classify(
    error: &wasmtime::Error,
    fatal: Option<(NodeId, String)>,
) -> (NodeFault, Option<NodeId>) {
    if let Some((node, message)) = fatal {
        return (NodeFault::Fatal { message }, Some(node));
    }
    if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
        return (NodeFault::FuelExhausted, None);
    }
    (
        NodeFault::WasmTrap {
            message: format!("{error:#}"),
        },
        None,
    )
}

/// Parses WAVE text as a value of type `ty`.
pub(crate) fn parse_wave(ty: &Type, text: &str) -> Result<Val, String> {
    Val::from_wave(ty, text).map_err(|e| format!("{e:#}"))
}

/// Checks that `val` structurally has value type `ty` (record field names
/// and order, case names, numeric widths). Handles and resources are never
/// port values.
pub(crate) fn check_type(ty: &Type, val: &Val) -> Result<(), String> {
    let mismatch = || {
        Err(format!(
            "expected {}, found {}",
            type_name(ty),
            val_name(val)
        ))
    };
    let payload = |ty: Option<Type>, val: &Option<Box<Val>>| match (ty, val) {
        (None, None) => Ok(()),
        (Some(ty), Some(val)) => check_type(&ty, val),
        (Some(_), None) => Err("missing payload".to_string()),
        (None, Some(_)) => Err("unexpected payload".to_string()),
    };
    match (ty, val) {
        (Type::Bool, Val::Bool(_))
        | (Type::S8, Val::S8(_))
        | (Type::U8, Val::U8(_))
        | (Type::S16, Val::S16(_))
        | (Type::U16, Val::U16(_))
        | (Type::S32, Val::S32(_))
        | (Type::U32, Val::U32(_))
        | (Type::S64, Val::S64(_))
        | (Type::U64, Val::U64(_))
        | (Type::Float32, Val::Float32(_))
        | (Type::Float64, Val::Float64(_))
        | (Type::Char, Val::Char(_))
        | (Type::String, Val::String(_)) => Ok(()),
        (Type::List(list), Val::List(items)) => {
            let elem = list.ty();
            items.iter().try_for_each(|item| check_type(&elem, item))
        }
        (Type::FixedLengthList(list), Val::FixedLengthList(items)) => {
            if items.len() != list.len() as usize {
                return Err(format!(
                    "expected {} elements, found {}",
                    list.len(),
                    items.len()
                ));
            }
            let elem = list.ty();
            items.iter().try_for_each(|item| check_type(&elem, item))
        }
        (Type::Map(map), Val::Map(entries)) => entries.iter().try_for_each(|(k, v)| {
            check_type(&map.key(), k)?;
            check_type(&map.value(), v)
        }),
        (Type::Record(record), Val::Record(fields)) => {
            if record.fields().len() != fields.len() {
                return mismatch();
            }
            record
                .fields()
                .zip(fields)
                .try_for_each(|(field, (name, value))| {
                    if field.name != name {
                        return Err(format!("expected field `{}`, found `{name}`", field.name));
                    }
                    check_type(&field.ty, value).map_err(|e| format!("field `{name}`: {e}"))
                })
        }
        (Type::Tuple(tuple), Val::Tuple(items)) => {
            if tuple.types().len() != items.len() {
                return mismatch();
            }
            tuple
                .types()
                .zip(items)
                .try_for_each(|(ty, item)| check_type(&ty, item))
        }
        (Type::Variant(variant), Val::Variant(name, value)) => {
            match variant.cases().find(|case| case.name == name) {
                Some(case) => payload(case.ty, value).map_err(|e| format!("case `{name}`: {e}")),
                None => Err(format!("no case `{name}`")),
            }
        }
        (Type::Enum(cases), Val::Enum(name)) => {
            if cases.names().any(|case| case == name) {
                Ok(())
            } else {
                Err(format!("no case `{name}`"))
            }
        }
        (Type::Option(option), Val::Option(value)) => match value {
            Some(value) => check_type(&option.ty(), value),
            None => Ok(()),
        },
        (Type::Result(result), Val::Result(value)) => match value {
            Ok(ok) => payload(result.ok(), ok),
            Err(err) => payload(result.err(), err),
        },
        (Type::Flags(flags), Val::Flags(set)) => match set
            .iter()
            .find(|flag| !flags.names().any(|name| name == flag.as_str()))
        {
            Some(flag) => Err(format!("no flag `{flag}`")),
            None => Ok(()),
        },
        _ => mismatch(),
    }
}

fn type_name(ty: &Type) -> &'static str {
    match ty {
        Type::Bool => "bool",
        Type::S8 => "s8",
        Type::U8 => "u8",
        Type::S16 => "s16",
        Type::U16 => "u16",
        Type::S32 => "s32",
        Type::U32 => "u32",
        Type::S64 => "s64",
        Type::U64 => "u64",
        Type::Float32 => "f32",
        Type::Float64 => "f64",
        Type::Char => "char",
        Type::String => "string",
        Type::List(_) => "list",
        Type::FixedLengthList(_) => "fixed-length list",
        Type::Map(_) => "map",
        Type::Record(_) => "record",
        Type::Tuple(_) => "tuple",
        Type::Variant(_) => "variant",
        Type::Enum(_) => "enum",
        Type::Option(_) => "option",
        Type::Result(_) => "result",
        Type::Flags(_) => "flags",
        Type::Own(_) | Type::Borrow(_) => "resource",
        Type::Future(_) => "future",
        Type::Stream(_) => "stream",
        Type::ErrorContext => "error-context",
    }
}

fn val_name(val: &Val) -> &'static str {
    match val {
        Val::Bool(_) => "bool",
        Val::S8(_) => "s8",
        Val::U8(_) => "u8",
        Val::S16(_) => "s16",
        Val::U16(_) => "u16",
        Val::S32(_) => "s32",
        Val::U32(_) => "u32",
        Val::S64(_) => "s64",
        Val::U64(_) => "u64",
        Val::Float32(_) => "f32",
        Val::Float64(_) => "f64",
        Val::Char(_) => "char",
        Val::String(_) => "string",
        Val::List(_) => "list",
        Val::FixedLengthList(_) => "fixed-length list",
        Val::Map(_) => "map",
        Val::Record(_) => "record",
        Val::Tuple(_) => "tuple",
        Val::Variant(..) => "variant",
        Val::Enum(_) => "enum",
        Val::Option(_) => "option",
        Val::Result(_) => "result",
        Val::Flags(_) => "flags",
        Val::Resource(_) => "resource",
        Val::Future(_) => "future",
        Val::Stream(_) => "stream",
        Val::ErrorContext(_) => "error-context",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload type of echo's optional `in` port, read from the
    /// instantiated component.
    async fn echo_in_type() -> Type {
        let engine = new_engine().unwrap();
        let component = Component::new(&engine, test_components::ECHO.wasm).unwrap();
        let node = NodeId::from("e");
        let linker = node_linker(&engine, &node, &|_| Ok(())).unwrap();
        let fuel = FuelPolicy {
            yield_interval: None,
            per_run: None,
        };
        let (_, signatures) = build_island(&engine, &[(&node, &component, &linker)], fuel)
            .await
            .unwrap();
        let fields = signatures[0].inputs.clone().unwrap();
        let (name, ty) = &fields[0];
        assert_eq!(name, "in");
        match ty {
            Type::Option(option) => option.ty(),
            other => panic!("`in` is optional, found {other:?}"),
        }
    }

    #[tokio::test]
    async fn wave_text_parses_against_component_types() {
        let ty = echo_in_type().await;
        assert_eq!(parse_wave(&ty, "1.5"), Ok(Val::Float64(1.5)));
        assert!(parse_wave(&ty, "\"wrong\"").is_err());
    }

    #[tokio::test]
    async fn values_are_checked_structurally() {
        let ty = echo_in_type().await;
        assert!(check_type(&ty, &Val::Float64(1.0)).is_ok());
        let err = check_type(&ty, &Val::U32(1)).unwrap_err();
        assert_eq!(err, "expected f64, found u32");
    }
}
