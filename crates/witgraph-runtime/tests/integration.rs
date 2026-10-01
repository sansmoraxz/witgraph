#![cfg(test)]
#![allow(missing_docs)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use test_components::{
    BUSY_LOOP, CONFIGURABLE, ECHO, Guest, MQTT_NODE, RELAY, STREAM_CONSUMER, STREAM_PRODUCER,
};
use wasmtime::StoreContextMut;
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, GraphBuilder, NodeId, PortRef, ResourceClaim,
};
use witgraph_runtime::{
    Debug, HostState, IslandSnapshot, NodeFault, NodePhase, RuntimeConfig, RuntimeError,
    RuntimeGraph, RuntimeMode, Snapshot, TickResult, TraceEvent, Val,
};

// ---- Fixtures ----

/// Lowers a guest's WIT into its contract.
fn contract(guest: Guest) -> ComponentContract {
    let mut contracts = witgraph_wit::load_components(guest.wit).expect("guest WIT lowers");
    assert_eq!(contracts.len(), 1, "one node world per guest");
    contracts.remove(0)
}

/// Contracts and component bytes for a set of guests.
struct Kit {
    contracts: Vec<ComponentContract>,
    wasm: HashMap<ComponentRef, &'static [u8]>,
}

impl Kit {
    fn new(guests: &[Guest]) -> Self {
        let mut kit = Kit {
            contracts: Vec::new(),
            wasm: HashMap::new(),
        };
        for guest in guests {
            let c = contract(*guest);
            kit.wasm.insert(c.id.clone(), guest.wasm);
            kit.contracts.push(c);
        }
        kit
    }

    /// The contract lowered from `guest`.
    fn get(&self, guest: Guest) -> ComponentRef {
        let id = contract(guest).id;
        assert!(
            self.contracts.iter().any(|c| c.id == id),
            "guest not in kit"
        );
        id
    }

    /// A builder with every kit component in its table.
    fn builder(&self, name: &str) -> GraphBuilder {
        self.contracts
            .iter()
            .fold(Graph::builder(name), |b, c| b.add_component(c))
    }

    async fn load<M: RuntimeMode>(
        &self,
        graph: Graph,
        config: RuntimeConfig,
        mode: M,
    ) -> RuntimeGraph<M> {
        let compiled = graph.compile(&self.contracts).expect("graph compiles");
        RuntimeGraph::load(compiled, &self.wasm, config, mode)
            .await
            .expect("graph loads")
    }
}

fn mqtt_linker(
    feed: Arc<Mutex<VecDeque<u32>>>,
) -> impl Fn(&mut wasmtime::component::Linker<HostState>) -> wasmtime::Result<()> {
    move |linker| {
        let feed = feed.clone();
        linker.instance("test:mqtt/source@0.1.0")?.func_wrap(
            "next-message",
            move |_store: StoreContextMut<'_, HostState>, (): ()| {
                Ok((feed.lock().unwrap().pop_front(),))
            },
        )?;
        Ok(())
    }
}

fn id(node: &str) -> NodeId {
    NodeId::from(node)
}

fn port(node: &str, port: &str) -> PortRef {
    PortRef::new(node, port)
}

fn read<M: RuntimeMode>(rt: &RuntimeGraph<M>, node: &str, port: &str) -> Option<Val> {
    rt.read_output(&id(node), &port.into())
}

fn read_f64<M: RuntimeMode>(rt: &RuntimeGraph<M>, node: &str, port: &str) -> f64 {
    match read(rt, node, port) {
        Some(Val::Float64(v)) => v,
        other => panic!("{node}.{port}: expected f64, got {other:?}"),
    }
}

fn read_u32<M: RuntimeMode>(rt: &RuntimeGraph<M>, node: &str, port: &str) -> u32 {
    match read(rt, node, port) {
        Some(Val::U32(v)) => v,
        other => panic!("{node}.{port}: expected u32, got {other:?}"),
    }
}

fn inject<M: RuntimeMode>(rt: &mut RuntimeGraph<M>, node: &str, port: &str, val: Val) {
    rt.inject(id(node), port.into(), val).expect("inject");
}

fn phase<M: RuntimeMode>(rt: &RuntimeGraph<M>, node: &str) -> NodePhase {
    rt.node_state(&id(node)).expect("node exists").phase()
}

/// Ticks until the graph is idle. Every tick must finish within 30s.
async fn settle<M: RuntimeMode>(rt: &mut RuntimeGraph<M>) -> usize {
    for n in 1..=100 {
        let result = tokio::time::timeout(Duration::from_secs(30), rt.tick())
            .await
            .expect("tick hung");
        match result {
            TickResult::Idle => return n,
            TickResult::Progress => {}
            other => panic!("unexpected tick result: {other:?}"),
        }
    }
    panic!("graph did not settle in 100 ticks");
}

fn runs_of(trace: &[TraceEvent], node: &str) -> usize {
    trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::RunStarted { node: n } if n.as_str() == node))
        .count()
}

fn faults(trace: &[TraceEvent]) -> Vec<&TraceEvent> {
    trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fault { .. }))
        .collect()
}

// ---- Value nodes ----

#[tokio::test]
async fn injected_inputs_feed_runs_and_absent_optionals_read_none() {
    let kit = Kit::new(&[ECHO]);
    let echo = kit.get(ECHO);
    let graph = kit
        .builder("echo")
        .add_node("a", echo.clone())
        .add_node("b", echo)
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(2.5));
    settle(&mut rt).await;

    assert_eq!(read_f64(&rt, "a", "out"), 2.5);
    assert_eq!(
        read_f64(&rt, "b", "out"),
        0.0,
        "absent optional input reads none"
    );
    assert_eq!(phase(&rt, "a"), NodePhase::Idle);
}

#[tokio::test]
async fn multi_port_inject() {
    let kit = Kit::new(&[CONFIGURABLE]);
    let graph = kit
        .builder("cfg")
        .add_node("cfg", kit.get(CONFIGURABLE))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "cfg", "rate", Val::U32(4));
    inject(&mut rt, "cfg", "gain", Val::Float64(3.0));
    settle(&mut rt).await;
    assert_eq!(read_f64(&rt, "cfg", "result"), 12.0);
}

#[tokio::test]
async fn values_propagate_and_unchanged_inputs_do_not_rerun() {
    let kit = Kit::new(&[ECHO]);
    let echo = kit.get(ECHO);
    let graph = kit
        .builder("chain")
        .add_node("a", echo.clone())
        .add_node("b", echo.clone())
        .add_node("c", echo)
        .connect("ab", port("a", "out"), port("b", "in"))
        .connect("bc", port("b", "out"), port("c", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0));
    settle(&mut rt).await;
    assert_eq!(read_f64(&rt, "c", "out"), 1.0);
    assert_eq!(
        runs_of(&rt.mode().trace(), "c"),
        1,
        "c waits for its ancestors"
    );

    rt.inject(id("a"), "in".into(), Val::Float64(2.0)).unwrap();
    settle(&mut rt).await;
    assert_eq!(read_f64(&rt, "c", "out"), 2.0);
    assert_eq!(runs_of(&rt.mode().trace(), "c"), 2);

    // Writing the same value again is not a change.
    rt.inject(id("a"), "in".into(), Val::Float64(2.0)).unwrap();
    assert!(matches!(rt.tick().await, TickResult::Idle));
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 2);
}

#[tokio::test]
async fn inject_rejects_bad_targets_and_types() {
    let kit = Kit::new(&[ECHO, STREAM_CONSUMER, STREAM_PRODUCER]);
    let graph = kit
        .builder("inject")
        .add_node("e", kit.get(ECHO))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Release).await;

    let err = rt.inject(id("e"), "in".into(), Val::U32(1)).unwrap_err();
    assert!(matches!(err, RuntimeError::ValueType { .. }), "{err}");
    let err = rt
        .inject(id("cons"), "items".into(), Val::U32(1))
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotAValuePort { .. }), "{err}");
    let err = rt
        .inject(id("ghost"), "in".into(), Val::Float64(1.0))
        .unwrap_err();
    assert!(matches!(err, RuntimeError::UnknownNode { .. }), "{err}");
}

use witgraph_runtime::Release;

// ---- Streams ----

fn pipeline(kit: &Kit) -> Graph {
    kit.builder("pipeline")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build()
}

/// Sets the producer's burst size and the consumer's take count; `None`
/// leaves the optional input absent (endless producer / take everything).
fn feed_pipeline<M: RuntimeMode>(rt: &mut RuntimeGraph<M>, burst: Option<u32>, take: Option<u32>) {
    if let Some(burst) = burst {
        inject(rt, "prod", "burst-size", Val::U32(burst));
    }
    if let Some(take) = take {
        inject(rt, "cons", "take", Val::U32(take));
    }
}

#[tokio::test]
async fn stream_producer_consumer_share_an_island() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    feed_pipeline(&mut rt, Some(5), None);
    assert_eq!(rt.compiled().islands().len(), 1);
    settle(&mut rt).await;

    assert_eq!(read_u32(&rt, "cons", "total"), 10);
    assert_eq!(read_u32(&rt, "cons", "count"), 5);
    assert_eq!(phase(&rt, "prod"), NodePhase::Idle);
    assert_eq!(phase(&rt, "cons"), NodePhase::Idle);
    let trace = rt.mode().trace();
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::GenerationFinished {
            island: 0,
            generation: 1
        }
    )));
    assert!(faults(&trace).is_empty(), "{:?}", faults(&trace));
}

#[tokio::test]
async fn backpressure_slow_consumer_gets_every_item() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    feed_pipeline(&mut rt, Some(2000), None);
    inject(&mut rt, "cons", "delay", Val::U32(3));
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "count"), 2000);
    assert_eq!(read_u32(&rt, "cons", "total"), (0..2000).sum::<u32>());
}

#[tokio::test]
async fn early_reader_drop_stops_an_infinite_producer() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    // No burst-size: the producer streams forever unless its reader goes.
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    feed_pipeline(&mut rt, None, Some(3));
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "count"), 3);
    assert_eq!(read_u32(&rt, "cons", "total"), 3);
    // The generation finished, so the producer's writer task exited.
    assert_eq!(phase(&rt, "prod"), NodePhase::Idle);
}

#[tokio::test]
async fn capability_import_feeds_a_stream() {
    let kit = Kit::new(&[MQTT_NODE, STREAM_CONSUMER]);
    let graph = kit
        .builder("mqtt")
        .add_node("mqtt", kit.get(MQTT_NODE))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("mqtt", "messages"), port("cons", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    assert_eq!(compiled.required_capabilities().len(), 1);
    let feed = Arc::new(Mutex::new(VecDeque::from([3u32, 4, 5, 6, 7])));
    let mut rt = RuntimeGraph::load_with_linker(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Release,
        mqtt_linker(feed.clone()),
    )
    .await
    .expect("loads with the capability");
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "count"), 5);
    assert_eq!(read_u32(&rt, "cons", "total"), 25);
    assert!(feed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wide_graph_full_flow() {
    let kit = Kit::new(&[
        ECHO,
        CONFIGURABLE,
        RELAY,
        STREAM_PRODUCER,
        STREAM_CONSUMER,
        MQTT_NODE,
    ]);
    let graph = kit
        .builder("wide")
        // configurable → echo → echo
        .add_node("cfg", kit.get(CONFIGURABLE))
        .add_node("echo_a", kit.get(ECHO))
        .add_node("echo_b", kit.get(ECHO))
        .connect("v1", port("cfg", "result"), port("echo_a", "in"))
        .connect("v2", port("echo_a", "out"), port("echo_b", "in"))
        // producer → consumer → relay (+1) → relay, plus a value fan-out
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .add_node("plus", kit.get(RELAY))
        .add_node("sink_a", kit.get(RELAY))
        .add_node("sink_b", kit.get(RELAY))
        .connect("s1", port("prod", "items"), port("cons", "items"))
        .connect("t1", port("cons", "total"), port("plus", "in"))
        .connect("f1", port("plus", "out"), port("sink_a", "in"))
        .connect("f2", port("plus", "out"), port("sink_b", "in"))
        // mqtt → consumer
        .add_node("mqtt", kit.get(MQTT_NODE))
        .add_node("count", kit.get(STREAM_CONSUMER))
        .connect("m1", port("mqtt", "messages"), port("count", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    // cfg, echo_a, echo_b, {prod, cons}, plus, sink_a, sink_b, {mqtt, count}
    assert_eq!(compiled.islands().len(), 8);
    let feed = Arc::new(Mutex::new((0..4u32).collect::<VecDeque<_>>()));
    let mut rt = RuntimeGraph::load_with_linker(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Debug::new(),
        mqtt_linker(feed),
    )
    .await
    .expect("loads");
    inject(&mut rt, "cfg", "rate", Val::U32(4));
    inject(&mut rt, "cfg", "gain", Val::Float64(3.0));
    inject(&mut rt, "prod", "burst-size", Val::U32(8));
    inject(&mut rt, "plus", "add", Val::U32(1));
    let ticks = settle(&mut rt).await;
    assert!(
        ticks <= 3,
        "no feedback, so one productive tick: took {ticks}"
    );

    assert_eq!(read_f64(&rt, "echo_b", "out"), 12.0, "4 * 3.0");
    assert_eq!(read_u32(&rt, "cons", "total"), 28, "sum(0..8)");
    assert_eq!(read_u32(&rt, "sink_a", "out"), 29);
    assert_eq!(read_u32(&rt, "sink_b", "out"), 29);
    assert_eq!(read_u32(&rt, "count", "count"), 4);
    let trace = rt.mode().trace();
    assert!(faults(&trace).is_empty(), "{:?}", faults(&trace));
    assert_eq!(
        runs_of(&trace, "sink_a"),
        1,
        "downstream waits for upstream"
    );
}

// ---- Feedback ----

#[tokio::test]
async fn feedback_loop_reruns_each_iteration() {
    let kit = Kit::new(&[RELAY]);
    let relay = kit.get(RELAY);
    let graph = kit
        .builder("loop")
        .add_node("a", relay.clone())
        .add_node("b", relay)
        .connect("ab", port("a", "out"), port("b", "in"))
        .connect_feedback("ba", port("b", "out"), port("a", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    // `a.in` is required: fed by the feedback edge, seeded by the host.
    inject(&mut rt, "a", "in", Val::U32(0));
    inject(&mut rt, "a", "add", Val::U32(1));
    for i in 1..=5u32 {
        assert!(matches!(rt.tick().await, TickResult::Progress));
        assert_eq!(read_u32(&rt, "b", "out"), i, "iteration {i}");
    }
    let trace = rt.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 5);
    assert_eq!(runs_of(&trace, "b"), 5);
}

// ---- Faults, fuel, fatal ----

#[tokio::test]
async fn a_trap_faults_only_its_island() {
    let kit = Kit::new(&[BUSY_LOOP, ECHO, STREAM_PRODUCER, STREAM_CONSUMER]);
    let graph = kit
        .builder("isolation")
        .add_node("busy", kit.get(BUSY_LOOP))
        .add_node("e", kit.get(ECHO))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into()));
    inject(&mut rt, "e", "in", Val::Float64(1.5));
    inject(&mut rt, "prod", "burst-size", Val::U32(3));
    settle(&mut rt).await;

    let busy = rt.node_state(&id("busy")).unwrap();
    assert_eq!(busy.phase(), NodePhase::Faulted);
    assert!(matches!(
        busy.fault_cause(),
        Some(NodeFault::WasmTrap { .. })
    ));
    assert_eq!(read_f64(&rt, "e", "out"), 1.5);
    assert_eq!(read_u32(&rt, "cons", "total"), 3);
    assert_eq!(phase(&rt, "cons"), NodePhase::Idle);
}

/// A pipeline plus a lone busy-loop node: two islands sharing one config.
fn pipeline_and_busy(kit: &Kit) -> Graph {
    let mut graph = pipeline(kit);
    graph.nodes.push(witgraph_ir::Node {
        id: id("busy"),
        component: kit.get(BUSY_LOOP),
        label: None,
        resources: Default::default(),
    });
    graph
}

#[tokio::test]
async fn busy_loop_exhausts_its_run_budget_while_a_sibling_island_finishes() {
    let kit = Kit::new(&[BUSY_LOOP, STREAM_PRODUCER, STREAM_CONSUMER]);
    let config = RuntimeConfig {
        yield_interval: Some(10_000),
        fuel_per_run: Some(5_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit
        .load(pipeline_and_busy(&kit), config, Debug::new())
        .await;
    feed_pipeline(&mut rt, Some(100), None);
    inject(&mut rt, "busy", "action", Val::Enum("spin".into()));
    settle(&mut rt).await;

    let busy = rt.node_state(&id("busy")).unwrap();
    assert!(
        matches!(busy.fault_cause(), Some(NodeFault::FuelExhausted)),
        "{:?}",
        busy.fault_cause()
    );
    assert_eq!(read_u32(&rt, "cons", "count"), 100);
    assert_eq!(read_u32(&rt, "cons", "total"), (0..100).sum::<u32>());
    assert_eq!(
        phase(&rt, "cons"),
        NodePhase::Idle,
        "the pipeline fits its budget"
    );
}

#[tokio::test]
async fn an_endless_island_exhausts_a_finite_budget() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let config = RuntimeConfig {
        fuel_per_run: Some(5_000_000),
        ..RuntimeConfig::default()
    };
    // No burst-size, no take: the generation never ends on its own, and the
    // budget is only reset when a `run` starts.
    let mut rt = kit.load(pipeline(&kit), config, Debug::new()).await;
    let tick = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .expect("fuel ends the endless generation");
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    for node in ["prod", "cons"] {
        let state = rt.node_state(&id(node)).unwrap();
        assert!(
            matches!(state.fault_cause(), Some(NodeFault::FuelExhausted)),
            "{node}: {:?}",
            state.fault_cause()
        );
    }
}

#[tokio::test]
async fn fatal_aborts_the_tick() {
    let kit = Kit::new(&[BUSY_LOOP]);
    let graph = kit
        .builder("fatal")
        .add_node("busy", kit.get(BUSY_LOOP))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("fatal".into()));
    match rt.tick().await {
        TickResult::Aborted { node, fault } => {
            assert_eq!(node, id("busy"));
            assert!(
                matches!(fault, NodeFault::Fatal { ref message } if message.contains("asked for it"))
            );
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
    assert_eq!(phase(&rt, "busy"), NodePhase::Faulted);
    assert!(
        matches!(rt.tick().await, TickResult::Idle),
        "a faulted island waits for input"
    );
}

#[tokio::test]
async fn a_faulted_island_restarts_on_new_input() {
    let kit = Kit::new(&[BUSY_LOOP]);
    let graph = kit
        .builder("restart")
        .add_node("busy", kit.get(BUSY_LOOP))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into()));
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Faulted);

    rt.inject(id("busy"), "action".into(), Val::Enum("finish".into()))
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Idle);
    assert_eq!(read_u32(&rt, "busy", "done"), 1);
    let trace = rt.mode().trace();
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::Restarted { node } if node.as_str() == "busy"))
    );
}

#[tokio::test]
async fn an_input_change_during_a_faulting_generation_reruns_after_the_fault() {
    let kit = Kit::new(&[BUSY_LOOP]);
    let graph = kit
        .builder("owed")
        .add_node("busy", kit.get(BUSY_LOOP))
        .build();
    let config = RuntimeConfig {
        yield_interval: Some(10_000),
        fuel_per_run: Some(200_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Debug::new()).await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into()));
    poll_tick_once(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Running);

    // Owed while the spinning generation runs; the fault must not drop it.
    inject(&mut rt, "busy", "action", Val::Enum("finish".into()));
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Idle);
    assert_eq!(read_u32(&rt, "busy", "done"), 1);
    let trace = rt.mode().trace();
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::Fault {
            fault: NodeFault::FuelExhausted,
            ..
        }
    )));
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::Restarted { node } if node.as_str() == "busy"))
    );
}

#[tokio::test]
async fn cancelling_a_faulted_island_marks_it_cancelled() {
    let kit = Kit::new(&[BUSY_LOOP]);
    let graph = kit
        .builder("cancel-faulted")
        .add_node("busy", kit.get(BUSY_LOOP))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into()));
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Faulted);

    rt.cancel(&id("busy")).unwrap();
    let state = rt.node_state(&id("busy")).unwrap();
    assert_eq!(state.phase(), NodePhase::Cancelled);
    assert!(state.fault_cause().is_none());
    rt.cancel(&id("busy")).unwrap();
    let cancels = rt
        .mode()
        .trace()
        .iter()
        .filter(|e| matches!(e, TraceEvent::Cancelled { .. }))
        .count();
    assert_eq!(cancels, 1, "cancelling a cancelled island does nothing");
}

// ---- Cancellation and shutdown ----

#[tokio::test]
async fn cancel_drops_an_endless_island_and_new_input_restarts_it() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    // Endless: no burst-size, no take.
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    let tick = tokio::time::timeout(Duration::from_millis(300), rt.tick()).await;
    assert!(
        tick.is_err(),
        "the endless generation keeps the tick running"
    );
    assert_eq!(phase(&rt, "cons"), NodePhase::Running);

    rt.cancel(&id("cons")).unwrap();
    assert_eq!(
        phase(&rt, "prod"),
        NodePhase::Cancelled,
        "cancel hits the whole island"
    );
    assert_eq!(phase(&rt, "cons"), NodePhase::Cancelled);
    assert!(matches!(rt.tick().await, TickResult::Idle));

    rt.inject(id("prod"), "burst-size".into(), Val::U32(4))
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "total"), 6, "0+1+2+3");
    assert_eq!(phase(&rt, "cons"), NodePhase::Idle);
    let trace = rt.mode().trace();
    for node in ["prod", "cons"] {
        assert!(
            trace
                .iter()
                .any(|e| matches!(e, TraceEvent::Cancelled { node: n } if n.as_str() == node))
        );
        assert!(
            trace
                .iter()
                .any(|e| matches!(e, TraceEvent::Restarted { node: n } if n.as_str() == node))
        );
    }
}

#[tokio::test]
async fn shutdown_cancels_everything() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER, ECHO]);
    let mut graph = pipeline(&kit);
    graph.nodes.push(witgraph_ir::Node {
        id: id("e"),
        component: kit.get(ECHO),
        label: None,
        resources: Default::default(),
    });
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Debug::new())
        .await;
    let _ = tokio::time::timeout(Duration::from_millis(200), rt.tick()).await;
    assert_eq!(phase(&rt, "e"), NodePhase::Idle);

    rt.shutdown().await;
    for node in ["prod", "cons", "e"] {
        assert_eq!(phase(&rt, node), NodePhase::Cancelled, "{node}");
    }
    assert!(matches!(rt.tick().await, TickResult::Idle));
}

// ---- Resources ----

fn two_claimants(kit: &Kit, a: f64, b: f64) -> Graph {
    let echo = kit.get(ECHO);
    kit.builder("resources")
        .add_node("a", echo.clone())
        .add_node("b", echo.clone())
        .add_node("free", echo)
        .set_resource("a", "disk", ResourceClaim::new(a).unwrap())
        .unwrap()
        .set_resource("b", "disk", ResourceClaim::new(b).unwrap())
        .unwrap()
        .build()
}

async fn load_claimants(kit: &Kit, a: f64, b: f64) -> RuntimeGraph<Debug> {
    let mut rt = kit
        .load(
            two_claimants(kit, a, b),
            RuntimeConfig::default(),
            Debug::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0));
    inject(&mut rt, "b", "in", Val::Float64(2.0));
    inject(&mut rt, "free", "in", Val::Float64(3.0));
    rt
}

/// Whether the generations of islands `x` and `y` ever overlapped.
fn overlapped(trace: &[TraceEvent], x: usize, y: usize) -> bool {
    let mut running = std::collections::HashSet::new();
    for event in trace {
        match event {
            TraceEvent::GenerationStarted { island, .. } => {
                running.insert(*island);
                if running.contains(&x) && running.contains(&y) {
                    return true;
                }
            }
            TraceEvent::GenerationFinished { island, .. } => {
                running.remove(island);
            }
            _ => {}
        }
    }
    false
}

#[tokio::test]
async fn resource_claims_exclude_each_other() {
    let kit = Kit::new(&[ECHO]);
    let mut rt = load_claimants(&kit, 0.6, 0.6).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&rt, "a", "out"), 1.0);
    assert_eq!(read_f64(&rt, "b", "out"), 2.0);
    assert_eq!(read_f64(&rt, "free", "out"), 3.0);
    let (ia, ib) = (
        rt.compiled().island_of(&id("a")).unwrap(),
        rt.compiled().island_of(&id("b")).unwrap(),
    );
    assert!(!overlapped(&rt.mode().trace(), ia, ib), "0.6 + 0.6 > 1.0");
}

#[tokio::test]
async fn compatible_claims_run_together() {
    let kit = Kit::new(&[ECHO]);
    let mut rt = load_claimants(&kit, 0.5, 0.5).await;
    settle(&mut rt).await;
    let (ia, ib) = (
        rt.compiled().island_of(&id("a")).unwrap(),
        rt.compiled().island_of(&id("b")).unwrap(),
    );
    assert!(overlapped(&rt.mode().trace(), ia, ib), "0.5 + 0.5 fits");
}

#[tokio::test]
async fn an_island_over_committing_a_resource_is_rejected_at_load() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut graph = pipeline(&kit);
    for node in &mut graph.nodes {
        node.resources
            .insert("disk".to_string().into(), ResourceClaim::new(0.6).unwrap());
    }
    let compiled = graph
        .compile(&kit.contracts)
        .expect("compiles: claims are per node");
    let err = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Release)
        .await
        .err()
        .expect("the island sums to 1.2");
    assert!(matches!(err, RuntimeError::InvalidConfig { .. }), "{err}");
}

// ---- Loading ----

#[tokio::test]
async fn bytes_of_another_component_are_rejected() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("mismatch").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let wasm = HashMap::from([(kit.get(ECHO), RELAY.wasm)]);
    let err = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Release)
        .await
        .err()
        .expect("relay bytes do not implement echo");
    assert!(
        matches!(err, RuntimeError::ContractMismatch { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn missing_bytes_are_rejected() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("missing").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let err = RuntimeGraph::load(compiled, &HashMap::new(), RuntimeConfig::default(), Release)
        .await
        .err()
        .expect("no bytes");
    assert!(matches!(err, RuntimeError::MissingWasm { .. }), "{err}");
}

// ---- Snapshots ----

fn counter_loop(kit: &Kit) -> Graph {
    let relay = kit.get(RELAY);
    kit.builder("loop")
        .add_node("a", relay.clone())
        .add_node("b", relay)
        .connect("ab", port("a", "out"), port("b", "in"))
        .connect_feedback("ba", port("b", "out"), port("a", "in"))
        .build()
}

#[tokio::test]
async fn restore_continues_a_feedback_loop_from_the_snapshot() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = kit
        .load(counter_loop(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "a", "in", Val::U32(0));
    inject(&mut rt, "a", "add", Val::U32(1));
    for _ in 0..3 {
        assert!(matches!(rt.tick().await, TickResult::Progress));
    }
    assert_eq!(read_u32(&rt, "b", "out"), 3);
    let snapshot = rt.snapshot().expect("quiescent");

    let mut fresh = kit
        .load(counter_loop(&kit), RuntimeConfig::default(), Debug::new())
        .await;
    fresh.restore(&snapshot).expect("same graph");
    assert_eq!(read_u32(&fresh, "b", "out"), 3, "latched outputs come back");
    assert!(matches!(fresh.tick().await, TickResult::Progress));
    assert_eq!(
        read_u32(&fresh, "b", "out"),
        4,
        "the loop continues where it stopped"
    );
    let trace = fresh.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 1);
    assert_eq!(runs_of(&trace, "b"), 1);
    assert_eq!(
        fresh.snapshot().unwrap().outputs[&id("b")][&"out".into()],
        "4"
    );
}

#[tokio::test]
async fn restore_onto_an_untouched_graph_runs_only_queued_islands() {
    let kit = Kit::new(&[ECHO]);
    let graph = || {
        kit.builder("pair")
            .add_node("a", kit.get(ECHO))
            .add_node("b", kit.get(ECHO))
            .build()
    };
    let mut rt = kit
        .load(graph(), RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0));
    settle(&mut rt).await;
    inject(&mut rt, "b", "in", Val::Float64(2.0));
    let snapshot = rt.snapshot().unwrap();
    assert!(snapshot.quiescent);
    assert_eq!(
        snapshot.islands,
        vec![IslandSnapshot {
            members: vec![id("b")],
            running: None,
            queued: true,
        }]
    );
    assert_eq!(snapshot.phases[&id("a")], NodePhase::Idle);

    let mut fresh = kit
        .load(graph(), RuntimeConfig::default(), Debug::new())
        .await;
    fresh.restore(&snapshot).unwrap();
    settle(&mut fresh).await;
    let trace = fresh.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 0, "a had no work outstanding");
    assert_eq!(runs_of(&trace, "b"), 1);
    assert_eq!(read_f64(&fresh, "a", "out"), 1.0);
    assert_eq!(read_f64(&fresh, "b", "out"), 2.0);
}

/// Polls one tick exactly once, leaving a long generation in flight.
async fn poll_tick_once<M: RuntimeMode>(rt: &mut RuntimeGraph<M>) {
    let polled = tokio::time::timeout(Duration::ZERO, rt.tick()).await;
    assert!(polled.is_err(), "the generation outlives one poll");
}

fn small_slices() -> RuntimeConfig {
    RuntimeConfig {
        yield_interval: Some(10_000),
        ..RuntimeConfig::default()
    }
}

#[tokio::test]
async fn a_mid_flight_snapshot_replays_the_streaming_generation() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Debug::new()).await;
    // Endless producer; the consumer stops after 5000 items.
    inject(&mut rt, "cons", "take", Val::U32(5000));
    inject(&mut rt, "cons", "delay", Val::U32(4));
    poll_tick_once(&mut rt).await;
    assert_eq!(phase(&rt, "cons"), NodePhase::Running);

    let snapshot = rt.snapshot().expect("snapshots work mid-generation");
    assert!(!snapshot.quiescent);
    assert_eq!(snapshot.phases[&id("cons")], NodePhase::Running);
    let [island] = snapshot.islands.as_slice() else {
        panic!("one island in flight: {:?}", snapshot.islands)
    };
    let started = island.running.as_ref().expect("in flight");
    assert_eq!(started[&id("cons")][&"take".into()], "5000");

    let err = rt.restore(&snapshot).unwrap_err();
    assert!(matches!(err, RuntimeError::NotQuiescent), "{err}");

    let mut fresh = kit.load(pipeline(&kit), small_slices(), Debug::new()).await;
    fresh
        .restore(&snapshot)
        .expect("fresh graph has nothing in flight");
    settle(&mut fresh).await;
    settle(&mut rt).await;
    let expected = (0..5000).sum::<u32>();
    for graph in [&rt, &fresh] {
        assert_eq!(read_u32(graph, "cons", "count"), 5000);
        assert_eq!(read_u32(graph, "cons", "total"), expected);
    }
    assert_eq!(
        runs_of(&fresh.mode().trace(), "cons"),
        1,
        "replayed once, not re-run"
    );
}

#[tokio::test]
async fn restore_after_shutdown_replays_the_interrupted_generation() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Debug::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(3000));
    inject(&mut rt, "cons", "delay", Val::U32(4));
    poll_tick_once(&mut rt).await;
    let snapshot = rt.snapshot().unwrap();

    rt.shutdown().await;
    rt.restore(&snapshot)
        .expect("nothing in flight after shutdown");
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "count"), 3000);
    assert_eq!(
        phase(&rt, "cons"),
        NodePhase::Idle,
        "the stopped island was rebuilt"
    );
}

#[tokio::test]
async fn restore_works_right_after_cancel() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Debug::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(3000));
    inject(&mut rt, "cons", "delay", Val::U32(4));
    poll_tick_once(&mut rt).await;
    let snapshot = rt.snapshot().unwrap();
    assert!(!snapshot.quiescent);

    rt.cancel(&id("cons")).unwrap();
    rt.restore(&snapshot)
        .expect("a cancelled island has nothing in flight");
    settle(&mut rt).await;
    assert_eq!(read_u32(&rt, "cons", "count"), 3000);
    assert_eq!(phase(&rt, "cons"), NodePhase::Idle);
}

#[tokio::test]
async fn restore_rejects_another_graph_and_bad_values() {
    let kit = Kit::new(&[ECHO, CONFIGURABLE]);
    let echo_graph = || kit.builder("e").add_node("n", kit.get(ECHO)).build();
    let mut rt = kit
        .load(echo_graph(), RuntimeConfig::default(), Debug::new())
        .await;
    inject(&mut rt, "n", "in", Val::Float64(1.5));
    settle(&mut rt).await;
    let snapshot = rt.snapshot().unwrap();

    let other = kit
        .builder("c")
        .add_node("n", kit.get(CONFIGURABLE))
        .build();
    let mut wrong = kit
        .load(other, RuntimeConfig::default(), Debug::new())
        .await;
    let err = wrong.restore(&snapshot).unwrap_err();
    assert!(
        matches!(err, RuntimeError::SnapshotMismatch { .. }),
        "{err}"
    );

    let mut bad = snapshot.clone();
    bad.inputs
        .get_mut(&id("n"))
        .unwrap()
        .insert("in".into(), "\"wrong\"".into());
    let mut fresh = kit
        .load(echo_graph(), RuntimeConfig::default(), Debug::new())
        .await;
    let err = fresh.restore(&bad).unwrap_err();
    assert!(matches!(err, RuntimeError::SnapshotValue { .. }), "{err}");

    let mut unknown = snapshot.clone();
    unknown
        .outputs
        .get_mut(&id("n"))
        .unwrap()
        .insert("nope".into(), "1".into());
    let err = fresh.restore(&unknown).unwrap_err();
    assert!(matches!(err, RuntimeError::SnapshotValue { .. }), "{err}");
    assert_eq!(
        fresh.read_output(&id("n"), &"out".into()),
        None,
        "a failed restore changes nothing"
    );
}

#[test]
fn snapshot_round_trips_through_serde() {
    let values = |port: &str, text: &str| -> witgraph_runtime::PortValues {
        [(
            id("n"),
            [(port.into(), text.to_string())].into_iter().collect(),
        )]
        .into_iter()
        .collect()
    };
    let snapshot = Snapshot {
        nodes: [(id("n"), "test:echo/echo@0.1.0".parse().unwrap())]
            .into_iter()
            .collect(),
        quiescent: false,
        phases: [(id("n"), NodePhase::Running)].into_iter().collect(),
        inputs: values("in", "1.5"),
        outputs: Default::default(),
        feedback: [("ba".into(), "3".to_string())].into_iter().collect(),
        islands: vec![IslandSnapshot {
            members: vec![id("n")],
            running: Some(values("in", "1.0")),
            queued: true,
        }],
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let back: Snapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(snapshot, back);
}

#[test]
fn guest_copy_of_the_runtime_wit_is_current() {
    let canonical = include_str!("../wit/witgraph-runtime.wit");
    let copy = std::fs::read_to_string(format!(
        "{}/deps/witgraph-runtime/witgraph-runtime.wit",
        BUSY_LOOP.wit
    ))
    .expect("busy-loop vendors the runtime WIT");
    assert_eq!(
        canonical, copy,
        "re-copy wit/witgraph-runtime.wit into the guest's deps"
    );
}
