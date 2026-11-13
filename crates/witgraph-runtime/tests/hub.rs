//! A generated hub, composed with its nodes and driven by hand, on wasmtime.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use std::sync::Mutex;
use test_components::{
    BLOB_RELAY, BLOB_SINK, BLOB_SOURCE, CONFIGURABLE, CORPUS_FLAT, CORPUS_RELAY, ECHO,
    FUTURE_READER, FUTURE_WRITER, Guest, MAYBE, RELAY, STREAM_CONSUMER, STREAM_PRODUCER,
    TENSOR_SINK, TENSOR_SOURCE,
};
use wasmtime::component::{
    Component, ComponentType, Func, Instance, Lift, Linker, Lower, Resource, ResourceType,
    TypedFunc, Val,
};
use wasmtime::{Engine, Store};
use witgraph_ir::{CompiledGraph, ComponentContract, Graph, PortRef};
use witgraph_wit::hub::{self, HubMember};
use witgraph_wit::load::WitSource;

#[derive(ComponentType, Lift, Lower, Debug, Clone, Default, PartialEq, Eq)]
#[component(record)]
struct Returned {
    #[component(name = "outputs-changed")]
    outputs_changed: Vec<u32>,
    #[component(name = "inputs-changed")]
    inputs_changed: Vec<u32>,
    #[component(name = "inputs-recorded")]
    inputs_recorded: Vec<u32>,
    #[component(name = "inputs-cleared")]
    inputs_cleared: Vec<u32>,
    #[component(name = "feedback-buffered")]
    feedback_buffered: Vec<u32>,
}

fn engine() -> Engine {
    witgraph_runtime::RuntimeConfig::default()
        .new_engine()
        .unwrap()
}

fn contract(guest: Guest) -> ComponentContract {
    guest.contract()
}

fn compose(compiled: &CompiledGraph, guests: &[(&str, Guest)]) -> Vec<u8> {
    let nodes: Vec<(witgraph_ir::NodeId, &[u8], WitSource, witgraph_ir::WorldRef)> = guests
        .iter()
        .map(|(node, guest)| {
            (
                witgraph_ir::NodeId::from(*node),
                guest.wasm,
                guest.wit_source(),
                guest.id().world,
            )
        })
        .collect();
    let members: Vec<HubMember<'_>> = nodes
        .iter()
        .map(|(node, bytes, wit, world)| HubMember {
            node,
            bytes,
            export: "node",
            wit,
            world,
            mocked: false,
        })
        .collect();
    hub::compose_graph(compiled, &members, hub::HubOptions::default())
        .unwrap()
        .bytes
}

struct Hub {
    store: Store<()>,
    instance: Instance,
}

impl Hub {
    async fn new(engine: &Engine, bytes: &[u8], linker: &Linker<()>) -> Self {
        let component = Component::new(engine, bytes).unwrap();
        let mut store = Store::new(engine, ());
        store.set_fuel(u64::MAX / 2).unwrap();
        store.fuel_async_yield_interval(Some(100_000)).unwrap();
        store.set_hostcall_fuel(usize::MAX / 2);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .unwrap();
        Self { store, instance }
    }

    fn func(&mut self, interface: &str, name: &str) -> Func {
        let at = self
            .instance
            .get_export_index(&mut self.store, None, interface)
            .unwrap();
        let at = self
            .instance
            .get_export_index(&mut self.store, Some(&at), name)
            .unwrap_or_else(|| panic!("no `{name}`"));
        self.instance.get_func(&mut self.store, at).unwrap()
    }

    fn run_fn(&mut self) -> TypedFunc<(u32,), (Returned,)> {
        self.func(hub::CONTROL_INTERFACE, "run")
            .typed(&self.store)
            .unwrap()
    }

    fn port<P, R>(&mut self, name: &str) -> TypedFunc<P, R>
    where
        P: wasmtime::component::ComponentNamedList + Lower,
        R: wasmtime::component::ComponentNamedList + Lift,
    {
        self.func(hub::PORTS_INTERFACE, name)
            .typed(&self.store)
            .unwrap()
    }
}

fn chain() -> (CompiledGraph, Vec<(&'static str, Guest)>) {
    let contracts = [
        contract(BLOB_SOURCE),
        contract(BLOB_RELAY),
        contract(BLOB_SINK),
    ];
    let port = |node: &str| PortRef::new(node, "blob");
    let compiled = contracts
        .iter()
        .fold(Graph::builder("chain"), |b, c| b.add_component(c))
        .add_node("source", contracts[0].id.clone())
        .add_node("a", contracts[1].id.clone())
        .add_node("b", contracts[1].id.clone())
        .add_node("sink", contracts[2].id.clone())
        .connect("source-a", port("source"), port("a"))
        .connect("a-b", port("a"), port("b"))
        .connect("b-sink", port("b"), port("sink"))
        .build()
        .compile(&contracts)
        .unwrap();
    (
        compiled,
        vec![
            ("source", BLOB_SOURCE),
            ("a", BLOB_RELAY),
            ("b", BLOB_RELAY),
            ("sink", BLOB_SINK),
        ],
    )
}

#[tokio::test]
async fn a_chain_moves_a_value_through_the_hub() {
    let (compiled, guests) = chain();
    let ports = witgraph_ir::PortTable::new(&compiled);
    let bytes = compose(&compiled, &guests);
    let engine = engine();
    let mut hub_ = Hub::new(&engine, &bytes, &Linker::new(&engine)).await;
    let run = hub_.run_fn();

    let size_in = ports.input(&"source".into(), &"size".into()).unwrap();
    let seed_in = ports.input(&"source".into(), &"seed".into()).unwrap();
    let blob_out = ports.output(&"b".into(), &"blob".into()).unwrap();
    let len_out = ports.output(&"sink".into(), &"len".into()).unwrap();
    let set_size = hub_.port::<(&[u32],), (bool,)>(&hub::set_input(size_in));
    let set_seed = hub_.port::<(&[u32],), (bool,)>(&hub::set_input(seed_in));
    let get_blob = hub_.port::<(), (Option<Vec<u8>>,)>(&hub::get_output(blob_out));
    let get_len = hub_.port::<(), (Option<u32>,)>(&hub::get_output(len_out));
    let order: HashMap<&str, u32> = ["source", "a", "b", "sink"]
        .iter()
        .map(|node| {
            let at = ports.node(&(*node).into()).unwrap();
            (*node, witgraph_ir::Idx::position(at) as u32)
        })
        .collect();

    let result = hub_
        .store
        .run_concurrent(async |acc| -> wasmtime::Result<_> {
            let call = async |node: &str| run.call_concurrent(acc, (order[node],)).await;
            assert_eq!(
                set_size.call_concurrent(acc, (&[1000u32][..],)).await?,
                (true,)
            );
            assert_eq!(
                set_seed.call_concurrent(acc, (&[3u32][..],)).await?,
                (true,)
            );
            assert_eq!(
                set_seed.call_concurrent(acc, (&[3u32][..],)).await?,
                (false,)
            );
            for node in ["source", "a", "b", "sink"] {
                let (returned,) = call(node).await?;
                assert!(!returned.outputs_changed.is_empty(), "{node}: {returned:?}");
            }
            let (blob,) = get_blob.call_concurrent(acc, ()).await?;
            let (len,) = get_len.call_concurrent(acc, ()).await?;
            // Same bytes again: nothing downstream of the source is touched.
            let (again,) = call("source").await?;
            Ok((blob, len, again))
        })
        .await
        .unwrap()
        .unwrap();
    let (blob, len, again) = result;
    let blob = blob.unwrap();
    assert_eq!(blob.len(), 1000);
    assert_eq!(blob[0], 3);
    assert_eq!(len, Some(1000));
    assert_eq!(again, Returned::default(), "equal output, nothing changed");
}

/// `guests` as a graph with `connect`ions and feedback, ready to drive.
fn graph(
    guests: &[(&'static str, Guest)],
    connect: &[(&str, &str, &str, &str, &str)],
    feedback: &[(&str, &str, &str, &str, &str)],
) -> CompiledGraph {
    let contracts: Vec<ComponentContract> = guests.iter().map(|(_, g)| contract(*g)).collect();
    let mut unique: Vec<ComponentContract> = Vec::new();
    for c in &contracts {
        if !unique.iter().any(|u| u.id == c.id) {
            unique.push(c.clone());
        }
    }
    let mut builder = unique
        .iter()
        .fold(Graph::builder("g"), |b, c| b.add_component(c));
    for ((node, _), c) in guests.iter().zip(&contracts) {
        builder = builder.add_node(*node, c.id.clone());
    }
    for (id, from, out, to, input) in connect {
        builder = builder.connect(*id, PortRef::new(*from, *out), PortRef::new(*to, *input));
    }
    for (id, from, out, to, input) in feedback {
        builder =
            builder.connect_feedback(*id, PortRef::new(*from, *out), PortRef::new(*to, *input));
    }
    builder.build().compile(&unique).unwrap()
}

/// Drives a hub through the dynamic path, with names.
struct Driver {
    hub: Hub,
    ports: witgraph_ir::PortTable,
}

impl Driver {
    async fn new(
        compiled: &CompiledGraph,
        guests: &[(&str, Guest)],
        linker: &Linker<()>,
        engine: &Engine,
    ) -> Self {
        let bytes = compose(compiled, guests);
        Self {
            hub: Hub::new(engine, &bytes, linker).await,
            ports: witgraph_ir::PortTable::new(compiled),
        }
    }

    fn member(&self, node: &str) -> u32 {
        witgraph_ir::Idx::position(self.ports.node(&node.into()).unwrap()) as u32
    }

    async fn run(&mut self, node: &str) -> Returned {
        let member = self.member(node);
        let run = self.hub.run_fn();
        self.hub
            .store
            .run_concurrent(async |acc| run.call_concurrent(acc, (member,)).await)
            .await
            .unwrap()
            .unwrap()
            .0
    }

    async fn set(&mut self, node: &str, port: &str, value: Val) -> bool {
        let input = self.ports.input(&node.into(), &port.into()).unwrap();
        let f = self.hub.func(hub::PORTS_INTERFACE, &hub::set_input(input));
        let mut results = [Val::Bool(false)];
        self.hub
            .store
            .run_concurrent(async |acc| {
                f.call_concurrent(acc, &[Val::List(vec![value])], &mut results)
                    .await
            })
            .await
            .unwrap()
            .unwrap();
        results[0] == Val::Bool(true)
    }

    async fn get_val(&mut self, name: &str) -> Option<Val> {
        let f = self.hub.func(hub::PORTS_INTERFACE, name);
        let mut results = [Val::Bool(false)];
        self.hub
            .store
            .run_concurrent(async |acc| f.call_concurrent(acc, &[], &mut results).await)
            .await
            .unwrap()
            .unwrap();
        match results.into_iter().next() {
            Some(Val::Option(value)) => value.map(|v| *v),
            other => panic!("not an option: {other:?}"),
        }
    }

    async fn output(&mut self, node: &str, port: &str) -> Option<Val> {
        let output = self.ports.output(&node.into(), &port.into()).unwrap();
        self.get_val(&hub::get_output(output)).await
    }

    async fn input(&mut self, node: &str, port: &str) -> Option<Val> {
        let input = self.ports.input(&node.into(), &port.into()).unwrap();
        self.get_val(&hub::get_input(input)).await
    }

    fn idx_in(&self, node: &str, port: &str) -> u32 {
        witgraph_ir::Idx::position(self.ports.input(&node.into(), &port.into()).unwrap()) as u32
    }

    fn idx_out(&self, node: &str, port: &str) -> u32 {
        witgraph_ir::Idx::position(self.ports.output(&node.into(), &port.into()).unwrap()) as u32
    }
}

#[tokio::test]
async fn a_value_flows_and_an_equal_one_stops_there() {
    let engine = engine();
    let guests = [("a", ECHO), ("b", ECHO)];
    let compiled = graph(&guests, &[("ab", "a", "out", "b", "in")], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    assert!(d.set("a", "in", Val::Float64(5.0)).await);
    assert!(!d.set("a", "in", Val::Float64(5.0)).await, "the same value");
    assert_eq!(d.input("a", "in").await, Some(Val::Float64(5.0)));
    assert_eq!(d.output("a", "out").await, None, "not run yet");

    let returned = d.run("a").await;
    assert_eq!(returned.outputs_changed, [d.idx_out("a", "out")]);
    assert_eq!(
        returned.inputs_changed,
        [d.idx_in("b", "in")],
        "b is in another group"
    );
    let returned = d.run("b").await;
    assert_eq!(returned.outputs_changed, [d.idx_out("b", "out")]);
    assert_eq!(d.output("b", "out").await, Some(Val::Float64(5.0)));
    assert_eq!(d.run("a").await, Returned::default(), "equal output");

    assert!(d.set("a", "in", Val::Float64(6.0)).await);
    let returned = d.run("a").await;
    assert_eq!(returned.inputs_changed, [d.idx_in("b", "in")]);
    assert_eq!(d.input("b", "in").await, Some(Val::Float64(6.0)));

    let clear = d.hub.func(hub::CONTROL_INTERFACE, "clear-input");
    let clear = clear.typed::<(u32,), (bool,)>(&d.hub.store).unwrap();
    let at = d.idx_in("a", "in");
    let cleared = d
        .hub
        .store
        .run_concurrent(async |acc| {
            let first = clear.call_concurrent(acc, (at,)).await?.0;
            let second = clear.call_concurrent(acc, (at,)).await?.0;
            Ok::<_, wasmtime::Error>((first, second))
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared, (true, false));
    assert_eq!(d.input("a", "in").await, None);
}

#[tokio::test]
async fn a_some_unwraps_into_an_optional_input_and_a_none_clears_it() {
    let engine = engine();
    let guests = [("m", MAYBE), ("c", CONFIGURABLE)];
    let compiled = graph(&guests, &[("mc", "m", "maybe", "c", "rate")], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    assert!(d.set("m", "x", Val::U32(9)).await);
    let returned = d.run("m").await;
    assert_eq!(returned.inputs_changed, [d.idx_in("c", "rate")]);
    assert_eq!(d.input("c", "rate").await, Some(Val::U32(9)));
    d.run("c").await;
    assert_eq!(d.output("c", "result").await, Some(Val::Float64(9.0)));

    let clear = d.hub.func(hub::CONTROL_INTERFACE, "clear-input");
    let clear = clear.typed::<(u32,), (bool,)>(&d.hub.store).unwrap();
    let x = d.idx_in("m", "x");
    d.hub
        .store
        .run_concurrent(async |acc| clear.call_concurrent(acc, (x,)).await)
        .await
        .unwrap()
        .unwrap();
    let returned = d.run("m").await;
    assert_eq!(
        returned.inputs_cleared,
        [d.idx_in("c", "rate")],
        "none clears it"
    );
    assert_eq!(d.input("c", "rate").await, None);
}

#[tokio::test]
async fn feedback_waits_for_its_latch() {
    let engine = engine();
    let guests = [("n", RELAY)];
    let compiled = graph(&guests, &[], &[("loop", "n", "out", "n", "in")]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    assert!(d.set("n", "in", Val::U32(1)).await);
    assert!(d.set("n", "add", Val::U32(1)).await);
    let returned = d.run("n").await;
    assert_eq!(returned.feedback_buffered, [0]);
    assert_eq!(
        d.input("n", "in").await,
        Some(Val::U32(1)),
        "not latched yet"
    );
    let conn = witgraph_ir::FeedbackIdx::from_position(0);
    assert_eq!(d.get_val(&hub::get_feedback(conn)).await, Some(Val::U32(2)));
    #[derive(ComponentType, Lift, Lower, Debug, PartialEq, Eq, Clone, Copy)]
    #[component(enum)]
    #[repr(u8)]
    #[allow(dead_code, reason = "lifted, never made here")]
    enum Latched {
        #[component(name = "unchanged")]
        Unchanged,
        #[component(name = "changed")]
        Changed,
        #[component(name = "cleared")]
        Cleared,
    }
    let latch = d.hub.func(hub::CONTROL_INTERFACE, "latch-feedback");
    let latch = latch.typed::<(u32,), (Latched,)>(&d.hub.store).unwrap();
    let (first, second) = d
        .hub
        .store
        .run_concurrent(async |acc| {
            let first = latch.call_concurrent(acc, (0,)).await?.0;
            let second = latch.call_concurrent(acc, (0,)).await?.0;
            Ok::<_, wasmtime::Error>((first, second))
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, Latched::Changed);
    assert_eq!(second, Latched::Unchanged, "nothing buffered any more");
    assert_eq!(d.input("n", "in").await, Some(Val::U32(2)));
    assert_eq!(d.get_val(&hub::get_feedback(conn)).await, None);
}

#[tokio::test]
async fn a_stream_passes_from_producer_to_reader_inside_the_hub() {
    let engine = engine();
    let guests = [("p", STREAM_PRODUCER), ("c", STREAM_CONSUMER)];
    let compiled = graph(&guests, &[("s", "p", "items", "c", "items")], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    assert!(d.set("p", "burst-size", Val::U32(100)).await);
    let returned = d.run("p").await;
    assert_eq!(returned.inputs_recorded, [d.idx_in("c", "items")]);
    let returned = d.run("c").await;
    assert!(!returned.outputs_changed.is_empty());
    assert_eq!(
        d.output("c", "total").await,
        Some(Val::U32((0..100u32).sum()))
    );
    // Again, with a fresh stream.
    assert!(d.set("p", "burst-size", Val::U32(10)).await);
    d.run("p").await;
    d.run("c").await;
    assert_eq!(
        d.output("c", "total").await,
        Some(Val::U32((0..10u32).sum()))
    );
}

#[tokio::test]
async fn a_future_passes_inside_the_hub_and_one_nothing_reads_is_dropped() {
    let engine = engine();
    let guests = [("w", FUTURE_WRITER), ("r", FUTURE_READER)];
    let compiled = graph(&guests, &[("f", "w", "done", "r", "data")], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    d.run("w").await;
    d.run("r").await;
    assert_eq!(d.output("r", "value").await, Some(Val::U32(7)));

    let guests = [("w", FUTURE_WRITER)];
    let compiled = graph(&guests, &[], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    d.run("w").await;
}

/// A value of the corpus's `values`, different as `n` differs.
fn corpus_value(n: u32) -> Val {
    let point = |x: f32, y: f64, tag: u8| {
        Val::Record(vec![
            ("x".into(), Val::Float32(x)),
            ("y".into(), Val::Float64(y)),
            ("tag".into(), Val::U8(tag)),
        ])
    };
    let string = |s: String| Val::String(s);
    let text = format!("text {n}");
    Val::Record(vec![
        ("boolean".into(), Val::Bool(n.is_multiple_of(2))),
        ("small".into(), Val::S8(-3)),
        ("medium".into(), Val::U16(n as u16)),
        ("word".into(), Val::S32(-(n as i32))),
        ("big".into(), Val::U64(u64::from(n) << 40)),
        ("real".into(), Val::Float32(n as f32 / 2.0)),
        ("double".into(), Val::Float64(f64::from(n) * 1.5)),
        ("letter".into(), Val::Char('é')),
        ("text".into(), string(text.clone())),
        (
            "bytes".into(),
            Val::List((0..n % 50).map(|i| Val::U8(i as u8)).collect()),
        ),
        (
            "numbers".into(),
            Val::List(vec![Val::Float32(0.5), Val::Float32(n as f32)]),
        ),
        (
            "words".into(),
            Val::List(vec![
                string("a".into()),
                string(text.clone()),
                string(String::new()),
            ]),
        ),
        (
            "color".into(),
            Val::Enum(["red", "green", "blue"][(n % 3) as usize].into()),
        ),
        (
            "perms".into(),
            Val::Flags(if n.is_multiple_of(2) {
                vec!["read".into(), "exec".into()]
            } else {
                vec![]
            }),
        ),
        ("point".into(), point(1.0, 2.0, n as u8)),
        (
            "packed".into(),
            Val::List(vec![Val::Record(vec![
                ("a".into(), Val::U16(1)),
                ("b".into(), Val::U16(n as u16)),
                ("c".into(), Val::U32(3)),
            ])]),
        ),
        (
            "shape".into(),
            match n % 5 {
                0 => Val::Variant("empty".into(), None),
                1 => Val::Variant("dot".into(), Some(Box::new(point(0.0, 0.0, 1)))),
                2 => Val::Variant("label".into(), Some(Box::new(string(text.clone())))),
                3 => Val::Variant(
                    "many".into(),
                    Some(Box::new(Val::List(vec![point(3.0, 4.0, 5)]))),
                ),
                _ => Val::Variant(
                    "nested".into(),
                    Some(Box::new(Val::Option(Some(Box::new(Val::List(vec![
                        string(text.clone()),
                    ])))))),
                ),
            },
        ),
        (
            "maybe".into(),
            Val::Option(if n.is_multiple_of(2) {
                Some(Box::new(point(5.0, 6.0, 7)))
            } else {
                None
            }),
        ),
        (
            "outcome".into(),
            Val::Result(if n.is_multiple_of(2) {
                Ok(Some(Box::new(string(text.clone()))))
            } else {
                Err(Some(Box::new(Val::Enum("blue".into()))))
            }),
        ),
        (
            "pair".into(),
            Val::Tuple(vec![Val::U8(1), string(text.clone())]),
        ),
        (
            "grid".into(),
            Val::List(vec![
                Val::List(vec![Val::U16(1), Val::U16(n as u16)]),
                Val::List(vec![]),
            ]),
        ),
        (
            "shapes".into(),
            Val::List(vec![
                Val::Variant("label".into(), Some(Box::new(string(text)))),
                Val::Variant("empty".into(), None),
            ]),
        ),
    ])
}

#[tokio::test]
async fn every_kind_of_value_crosses_the_hub_unchanged() {
    let engine = engine();
    let guests = [("a", CORPUS_RELAY), ("b", CORPUS_RELAY)];
    let compiled = graph(
        &guests,
        &[
            ("v", "a", "value", "b", "value"),
            ("o", "a", "other", "b", "other"),
        ],
        &[],
    );
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    for n in 0..20 {
        let value = corpus_value(n);
        assert!(d.set("a", "value", value.clone()).await, "n={n}");
        assert!(!d.set("a", "value", value.clone()).await, "equal, n={n}");
        let returned = d.run("a").await;
        assert_eq!(
            returned.outputs_changed.len(),
            1 + usize::from(n == 0),
            "n={n}: {returned:?}"
        );
        d.run("b").await;
        assert_eq!(
            d.output("b", "value").await,
            Some(Val::Option(Some(Box::new(value.clone())))),
            "n={n}"
        );
        assert_eq!(d.run("a").await, Returned::default(), "equal output, n={n}");
    }
}

#[tokio::test]
async fn nans_are_one_value_and_zeroes_two() {
    let engine = engine();
    let guests = [("a", ECHO)];
    let compiled = graph(&guests, &[], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    assert!(d.set("a", "in", Val::Float64(f64::NAN)).await);
    assert!(
        !d.set("a", "in", Val::Float64(-f64::NAN)).await,
        "NaN is NaN"
    );
    assert!(d.set("a", "in", Val::Float64(0.0)).await);
    assert!(
        d.set("a", "in", Val::Float64(-0.0)).await,
        "0.0 is not -0.0"
    );
    assert!(!d.set("a", "in", Val::Float64(-0.0)).await);
}

#[tokio::test]
async fn a_small_record_is_passed_flat_whatever_its_cases_share() {
    let engine = engine();
    let guests = [("a", CORPUS_FLAT)];
    let compiled = graph(&guests, &[], &[]);
    let mut d = Driver::new(&compiled, &guests, &Linker::new(&engine), &engine).await;
    let cases = [
        Val::Variant("whole".into(), Some(Box::new(Val::U64(u64::MAX - 5)))),
        Val::Variant("ratio".into(), Some(Box::new(Val::Float32(1.25)))),
        Val::Variant("name".into(), Some(Box::new(Val::String("a name".into())))),
        Val::Variant("nothing".into(), None),
    ];
    for case in cases {
        assert!(d.set("a", "mixed", case.clone()).await);
        d.run("a").await;
        assert_eq!(
            d.output("a", "mixed").await,
            Some(Val::Option(Some(Box::new(case))))
        );
    }
}

/// What a limiter saw of the hub's memory: the one memory that starts at one
/// page (a Rust guest's starts over a megabyte), and how often it grew. Guests'
/// own memories are theirs: `corpus-relay` grows slowly called directly too.
#[derive(Default)]
struct HubMemory {
    size: usize,
    grown: usize,
}

impl wasmtime::ResourceLimiter for HubMemory {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if (current == 0 && desired == 65536) || (self.size != 0 && current == self.size) {
            self.size = desired;
            self.grown += 1;
        }
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

/// `node`'s `name` in the hub's interface.
fn func_in(store: &mut Store<HubMemory>, instance: &Instance, interface: &str, name: &str) -> Func {
    let at = instance
        .get_export_index(&mut *store, None, interface)
        .unwrap();
    let at = instance
        .get_export_index(&mut *store, Some(&at), name)
        .unwrap();
    instance.get_func(&mut *store, at).unwrap()
}

async fn hub_store(engine: &Engine, bytes: &[u8]) -> (Store<HubMemory>, Instance) {
    let component = Component::new(engine, bytes).unwrap();
    let mut store = Store::new(engine, HubMemory::default());
    store.limiter(|memory| memory as &mut dyn wasmtime::ResourceLimiter);
    store.set_fuel(u64::MAX / 2).unwrap();
    store.set_hostcall_fuel(usize::MAX / 2);
    let linker = Linker::<HubMemory>::new(engine);
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .unwrap();
    (store, instance)
}

#[tokio::test]
async fn the_hub_uses_again_what_it_frees_values_of_every_kind() {
    let engine = engine();
    let guests = [("a", CORPUS_RELAY), ("b", CORPUS_RELAY)];
    let compiled = graph(
        &guests,
        &[
            ("v", "a", "value", "b", "value"),
            ("o", "a", "other", "b", "other"),
        ],
        &[],
    );
    let bytes = compose(&compiled, &guests);
    let ports = witgraph_ir::PortTable::new(&compiled);
    let (mut store, instance) = hub_store(&engine, &bytes).await;
    let input = ports.input(&"a".into(), &"value".into()).unwrap();
    let set = func_in(
        &mut store,
        &instance,
        hub::PORTS_INTERFACE,
        &hub::set_input(input),
    );
    let run = func_in(&mut store, &instance, hub::CONTROL_INTERFACE, "run")
        .typed::<(u32,), (Returned,)>(&store)
        .unwrap();
    let member = |n: &str| witgraph_ir::Idx::position(ports.node(&n.into()).unwrap()) as u32;
    let (a, b) = (member("a"), member("b"));
    let mut after_warmup = 0;
    for round in 0..4 {
        for n in 0..200 {
            let value = corpus_value(n);
            store
                .run_concurrent(async |acc| -> wasmtime::Result<()> {
                    let mut result = [Val::Bool(false)];
                    set.call_concurrent(acc, &[Val::List(vec![value])], &mut result)
                        .await?;
                    run.call_concurrent(acc, (a,)).await?;
                    run.call_concurrent(acc, (b,)).await?;
                    Ok(())
                })
                .await
                .unwrap()
                .unwrap();
        }
        if round == 0 {
            after_warmup = store.data().grown;
        }
    }
    assert!(after_warmup > 0, "the hub's memory was found");
    assert_eq!(store.data().grown, after_warmup);
}

#[tokio::test]
async fn the_hub_uses_again_what_it_frees_values_of_any_size() {
    let engine = engine();
    let (compiled, guests) = chain();
    let bytes = compose(&compiled, &guests);
    let ports = witgraph_ir::PortTable::new(&compiled);
    let (mut store, instance) = hub_store(&engine, &bytes).await;
    let set = |store: &mut Store<HubMemory>, port: &str| {
        let input = ports.input(&"source".into(), &port.into()).unwrap();
        func_in(
            store,
            &instance,
            hub::PORTS_INTERFACE,
            &hub::set_input(input),
        )
        .typed::<(Vec<u32>,), (bool,)>(&*store)
        .unwrap()
    };
    let (set_size, set_seed) = (set(&mut store, "size"), set(&mut store, "seed"));
    let run = func_in(&mut store, &instance, hub::CONTROL_INTERFACE, "run")
        .typed::<(u32,), (Returned,)>(&store)
        .unwrap();
    let mut after_warmup = 0;
    for round in 0..4u32 {
        for (seed, size) in [1u32, 100, 70_000, 5, 1 << 20, 300_000, 0, 1 << 18]
            .into_iter()
            .enumerate()
        {
            store
                .run_concurrent(async |acc| -> wasmtime::Result<()> {
                    set_size.call_concurrent(acc, (vec![size],)).await?;
                    set_seed
                        .call_concurrent(acc, (vec![seed as u32 + round],))
                        .await?;
                    for member in 0..4 {
                        run.call_concurrent(acc, (member,)).await?;
                    }
                    Ok(())
                })
                .await
                .unwrap()
                .unwrap();
        }
        if round == 0 {
            after_warmup = store.data().size;
        }
    }
    assert!(
        after_warmup >= 1 << 20,
        "it held a megabyte: {after_warmup}"
    );
    assert_eq!(store.data().size, after_warmup, "and never needed more");
}

/// `test:data/tensor` in the host, enough for the tensor source and sink.
struct TensorObject;
static TENSORS: Mutex<Vec<Option<Vec<u8>>>> = Mutex::new(Vec::new());
static HOST_FATAL: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code, reason = "lifted, never made here")]
enum Dtype {
    #[component(name = "uint8")]
    Uint8,
    #[component(name = "float32")]
    Float32,
}

fn tensor_linker(engine: &Engine, nodes: usize) -> Linker<()> {
    let mut linker = Linker::<()>::new(engine);
    let mut tensor = linker.instance("test:data/tensor@0.1.0").unwrap();
    tensor
        .resource("tensor", ResourceType::host::<TensorObject>(), |_, rep| {
            TENSORS.lock().unwrap()[rep as usize] = None;
            Ok(())
        })
        .unwrap();
    tensor
        .func_wrap_concurrent(
            "[static]tensor.from-bytes",
            |_, (_, _, data): (Dtype, Vec<u32>, Vec<u8>)| {
                Box::pin(async move {
                    let mut tensors = TENSORS.lock().unwrap();
                    tensors.push(Some(data));
                    let rep = tensors.len() as u32 - 1;
                    Ok((Ok::<_, String>(Resource::<TensorObject>::new_own(rep)),))
                })
            },
        )
        .unwrap();
    tensor
        .func_wrap_concurrent(
            "[method]tensor.read",
            |_, (it, offset, len): (Resource<TensorObject>, u64, u64)| {
                Box::pin(async move {
                    let tensors = TENSORS.lock().unwrap();
                    let data = tensors[it.rep() as usize].clone().unwrap_or_default();
                    let start = (offset as usize).min(data.len());
                    let end = (start + len as usize).min(data.len());
                    Ok((data[start..end].to_vec(),))
                })
            },
        )
        .unwrap();
    tensor
        .func_wrap(
            "[method]tensor.shape",
            |_, (it,): (Resource<TensorObject>,)| {
                let tensors = TENSORS.lock().unwrap();
                let len = tensors[it.rep() as usize].as_ref().map_or(0, Vec::len);
                Ok((vec![len as u32],))
            },
        )
        .unwrap();
    let mut lend = linker.instance(hub::LEND_INTERFACE).unwrap();
    lend.func_wrap("lend-handle-0", |_, (it,): (Resource<TensorObject>,)| {
        HOST_FATAL.lock().unwrap().push(it.rep() as usize);
        Ok(())
    })
    .unwrap();
    for at in 0..nodes {
        let node = at;
        let mut host = linker.instance(&hub::host_import(at)).unwrap();
        host.func_wrap(
            "fatal",
            move |_, (message,): (String,)| -> wasmtime::Result<()> {
                Err(wasmtime::format_err!("node {node}: {message}"))
            },
        )
        .unwrap();
    }
    linker
}

#[tokio::test]
async fn a_handle_is_held_by_the_hub_lent_to_its_reader_and_dropped_once() {
    let engine = engine();
    let guests = [("s", TENSOR_SOURCE), ("k", TENSOR_SINK)];
    let compiled = graph(&guests, &[("t", "s", "tensor", "k", "tensor")], &[]);
    let linker = tensor_linker(&engine, 2);
    let mut d = Driver::new(&compiled, &guests, &linker, &engine).await;
    let source_at = d.idx_out("s", "tensor");
    assert_eq!(source_at, 0, "the lend the linker is told of");

    let live = || TENSORS.lock().unwrap().iter().flatten().count();
    let before = live();
    assert!(d.set("s", "width", Val::U32(4)).await);
    assert!(d.set("s", "height", Val::U32(2)).await);
    let returned = d.run("s").await;
    assert_eq!(
        returned.outputs_changed,
        [source_at],
        "a handle is always new"
    );
    assert_eq!(returned.inputs_changed, [d.idx_in("k", "tensor")]);
    d.run("k").await;
    d.run("k").await;
    assert_eq!(d.output("k", "len").await, Some(Val::U64(4 * 2 * 3)));
    assert_eq!(live(), before + 1);

    let read = d.hub.func(
        hub::PORTS_INTERFACE,
        &hub::read_handle(witgraph_ir::OutputIdx::from_position(0)),
    );
    let read = read.typed::<(), ()>(&d.hub.store).unwrap();
    d.hub
        .store
        .run_concurrent(async |acc| read.call_concurrent(acc, ()).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(HOST_FATAL.lock().unwrap().len(), 1, "lent to the host");

    // A new one replaces it: the old one is dropped, while the reader's
    // borrow of it was over long ago.
    assert!(d.set("s", "seed", Val::U32(1)).await);
    d.run("s").await;
    assert_eq!(live(), before + 1);
}

#[tokio::test]
async fn each_node_has_a_host_import_of_its_own() {
    let engine = engine();
    let guests = [("s", TENSOR_SOURCE), ("k", TENSOR_SINK)];
    let compiled = graph(&guests, &[("t", "s", "tensor", "k", "tensor")], &[]);
    let bytes = compose(&compiled, &guests);
    let component = Component::new(&engine, &bytes).unwrap();
    let imports: Vec<String> = component
        .component_type()
        .imports(&engine)
        .map(|(name, _)| name.to_string())
        .collect();
    assert!(imports.contains(&hub::host_import(0)), "{imports:?}");
    assert!(
        imports.contains(&"test:data/tensor@0.1.0".to_string()),
        "{imports:?}"
    );
    assert!(
        !imports
            .iter()
            .any(|name| name.starts_with("witgraph:runtime/host")),
        "{imports:?}"
    );
}

#[test]
fn the_same_graph_makes_the_same_bytes() {
    let (compiled, guests) = chain();
    assert_eq!(compose(&compiled, &guests), compose(&compiled, &guests));
}
