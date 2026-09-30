#![allow(missing_docs)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use witgraph_ir::{
    Capability, ComponentContract, Graph, NodeId, PortDef, PortKind, PortName, PortRef, Type, Val,
};
use witgraph_runtime::{
    val_to_bytes, Debug, NodePhase, RuntimeConfig, RuntimeGraph, TickResult, TraceEvent,
};

fn echo_cref() -> witgraph_ir::ComponentRef {
    "test:components/echo@0.1.0".parse().unwrap()
}

fn configurable_cref() -> witgraph_ir::ComponentRef {
    "test:components/configurable@0.1.0".parse().unwrap()
}

fn echo_contract() -> ComponentContract {
    ComponentContract {
        id: echo_cref(),
        inputs: vec![PortDef::new("in", PortKind::Value, Type::F64).optional()],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::F64)],
        capabilities: vec![],
        type_names: vec![],
        docs: None,
    }
}

fn configurable_contract() -> ComponentContract {
    ComponentContract {
        id: configurable_cref(),
        inputs: vec![
            PortDef::new("rate", PortKind::Value, Type::U32).optional(),
            PortDef::new("gain", PortKind::Value, Type::F64).optional(),
        ],
        outputs: vec![PortDef::new("result", PortKind::Value, Type::F64)],
        capabilities: vec![],
        type_names: vec![],
        docs: None,
    }
}

fn u32_echo_cref() -> witgraph_ir::ComponentRef {
    "test:components/u32-echo@0.1.0".parse().unwrap()
}

fn u32_echo_contract() -> ComponentContract {
    ComponentContract {
        id: u32_echo_cref(),
        inputs: vec![PortDef::new("in", PortKind::Value, Type::U32).optional()],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::U32)],
        capabilities: vec![],
        type_names: vec![],
        docs: None,
    }
}

fn echo_wasm_map() -> HashMap<witgraph_ir::ComponentRef, &'static [u8]> {
    HashMap::from([(echo_cref(), test_components::ECHO)])
}

fn stream_producer_cref() -> witgraph_ir::ComponentRef {
    "test:components/stream-producer@0.1.0".parse().unwrap()
}

fn stream_consumer_cref() -> witgraph_ir::ComponentRef {
    "test:components/stream-consumer@0.1.0".parse().unwrap()
}

fn mqtt_node_cref() -> witgraph_ir::ComponentRef {
    "test:components/mqtt-node@0.1.0".parse().unwrap()
}

fn stream_producer_contract() -> ComponentContract {
    ComponentContract {
        id: stream_producer_cref(),
        inputs: vec![PortDef::new("burst-size", PortKind::Value, Type::U32).optional()],
        outputs: vec![PortDef::new("items", PortKind::Stream, Type::U32)],
        capabilities: vec![],
        type_names: vec![],
        docs: None,
    }
}

fn stream_consumer_contract() -> ComponentContract {
    ComponentContract {
        id: stream_consumer_cref(),
        inputs: vec![PortDef::new("items", PortKind::Stream, Type::U32)],
        outputs: vec![PortDef::new("total", PortKind::Value, Type::U32)],
        capabilities: vec![],
        type_names: vec![],
        docs: None,
    }
}

fn mqtt_node_contract() -> ComponentContract {
    ComponentContract {
        id: mqtt_node_cref(),
        inputs: vec![],
        outputs: vec![PortDef::new("count", PortKind::Value, Type::U32)],
        capabilities: vec![Capability::new("witgraph:runtime/mqtt-source@0.1.0")],
        type_names: vec![],
        docs: None,
    }
}

fn all_wasm_map() -> HashMap<witgraph_ir::ComponentRef, &'static [u8]> {
    HashMap::from([
        (echo_cref(), test_components::ECHO),
        (u32_echo_cref(), test_components::ECHO),
        (configurable_cref(), test_components::CONFIGURABLE),
        (stream_producer_cref(), test_components::STREAM_PRODUCER),
        (stream_consumer_cref(), test_components::STREAM_CONSUMER),
        (mqtt_node_cref(), test_components::MQTT_NODE),
    ])
}

fn read_f64<M: witgraph_runtime::RuntimeMode>(rt: &RuntimeGraph<M>, node: &str, port: &str) -> f64 {
    match rt.read_output(&NodeId::from(node), &PortName::from(port)) {
        Some(Val::F64(v)) => *v,
        other => panic!("expected F64 at {node}.{port}, got {other:?}"),
    }
}

fn read_u32_output<M: witgraph_runtime::RuntimeMode>(
    rt: &RuntimeGraph<M>,
    node: &str,
    port: &str,
) -> u32 {
    match rt.read_output(&NodeId::from(node), &PortName::from(port)) {
        Some(Val::U32(v)) => *v,
        other => panic!("expected U32 at {node}.{port}, got {other:?}"),
    }
}

async fn tick_until_idle<M: witgraph_runtime::RuntimeMode>(rt: &mut RuntimeGraph<M>) {
    for _ in 0..100 {
        match rt.tick().await {
            TickResult::Idle | TickResult::Completed => return,
            TickResult::Progress => continue,
            other => panic!("unexpected tick result: {other:?}"),
        }
    }
    panic!("did not reach idle within 100 ticks");
}

/// echo with `e.out → sink.in` so `read_output("e", "out")` has a
/// channel. `e.in` stays unconnected — config serves it via the
/// snapshot fallback.
fn echo_with_sink() -> Graph {
    Graph::builder("t")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build()
}

#[tokio::test]
async fn echo_node_activates() {
    let graph = echo_with_sink();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "e", "out"), 0.0, "default when no config");
}

#[tokio::test]
async fn config_seeds_before_first_activation() {
    let mut graph = echo_with_sink();
    graph.nodes.iter_mut().find(|n| n.id == NodeId::from("e")).unwrap()
        .config.insert(PortName::from("in"), Val::F64(99.0));
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "e", "out"), 99.0);
}

#[tokio::test]
async fn config_via_builder() {
    let graph = Graph::builder("t")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("e", "in", Val::F64(7.0)).unwrap()
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "e", "out"), 7.0);
}

#[tokio::test]
async fn snapshot_round_trips() {
    let wasm = echo_wasm_map();

    let graph = Graph::builder("snap-test")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("e", "in", Val::F64(42.0)).unwrap()
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;
    assert_eq!(read_f64(&rt, "e", "out"), 42.0);

    let snap = rt.snapshot_values();
    assert_eq!(
        snap.get(&NodeId::from("e"))
            .and_then(|p| p.get(&PortName::from("in"))),
        Some(&Val::F64(42.0)),
    );

    let mut graph2 = Graph::builder("snap-restore")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    graph2.apply_snapshot(&snap);
    let compiled2 = graph2.compile().expect("valid graph");
    let mut rt2 = RuntimeGraph::load(
        compiled2,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt2).await;
    assert_eq!(
        read_f64(&rt2, "e", "out"),
        42.0,
        "restored from snapshot"
    );
}

#[tokio::test]
async fn unconnected_optional_port_reads_config() {
    let graph = Graph::builder("unconnected-test")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("e", "in", Val::F64(55.5)).unwrap()
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;
    assert_eq!(read_f64(&rt, "e", "out"), 55.5);
}

#[tokio::test]
async fn multi_port_config() {
    let graph = Graph::builder("multi-config-test")
        .add_component(configurable_contract())
        .add_component(echo_contract())
        .add_node("c", configurable_cref())
        .add_node("sink", echo_cref())
        .set_config("c", "rate", Val::U32(5)).unwrap()
        .set_config("c", "gain", Val::F64(2.5)).unwrap()
        .connect(
            "c1",
            PortRef::new("c", "result"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &all_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;
    assert_eq!(read_f64(&rt, "c", "result"), 12.5); // 5 * 2.5
}

#[tokio::test]
async fn connected_nodes_propagate() {
    let graph = Graph::builder("chain-test")
        .add_component(echo_contract())
        .add_node("src", echo_cref())
        .add_node("dst", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("src", "in", Val::F64(3.14)).unwrap()
        .connect(
            "c1",
            PortRef::new("src", "out"),
            PortRef::new("dst", "in"),
        )
        .connect(
            "c2",
            PortRef::new("dst", "out"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "src", "out"), 3.14, "source output");
    assert_eq!(read_f64(&rt, "dst", "out"), 3.14, "propagated through chain");
}

// ---- Stream / async tests ----

async fn tick_until_done<M: witgraph_runtime::RuntimeMode>(rt: &mut RuntimeGraph<M>) -> usize {
    let mut ticks = 0;
    loop {
        ticks += 1;
        assert!(ticks < 200, "did not complete within 200 ticks");
        match rt.tick().await {
            TickResult::Completed => return ticks,
            TickResult::Idle => return ticks,
            TickResult::Progress => continue,
            other => panic!("unexpected tick result: {other:?}"),
        }
    }
}

fn stream_wasm_map() -> HashMap<witgraph_ir::ComponentRef, &'static [u8]> {
    HashMap::from([
        (stream_producer_cref(), test_components::STREAM_PRODUCER),
        (stream_consumer_cref(), test_components::STREAM_CONSUMER),
        (echo_cref(), test_components::ECHO),
        (u32_echo_cref(), test_components::ECHO),
    ])
}

#[tokio::test]
async fn stream_producer_consumer() {
    let graph = Graph::builder("stream-test")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(5)).unwrap()
        .connect(
            "s1",
            PortRef::new("prod", "items"),
            PortRef::new("cons", "items"),
        )
        .connect(
            "v1",
            PortRef::new("cons", "total"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let mut rt = RuntimeGraph::load(
        compiled,
        &stream_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_done(&mut rt).await;

    assert_eq!(read_u32_output(&rt, "cons", "total"), 10); // 0+1+2+3+4
}

#[tokio::test]
async fn backpressure_suspends_and_resumes() {
    let graph = Graph::builder("bp-test")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(8)).unwrap()
        .connect(
            "s1",
            PortRef::new("prod", "items"),
            PortRef::new("cons", "items"),
        )
        .connect(
            "v1",
            PortRef::new("cons", "total"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &stream_wasm_map(),
        RuntimeConfig {
            channel_capacity: 2,
            ..RuntimeConfig::default()
        },
        debug,
    )
    .await
    .expect("load");

    let ticks = tick_until_done(&mut rt).await;
    assert!(ticks > 1, "backpressure should require multiple ticks");

    assert_eq!(read_u32_output(&rt, "cons", "total"), 28); // 0+1+2+3+4+5+6+7

    let trace = rt.mode().trace();
    let suspended = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::PhaseTransition {
                node,
                to: NodePhase::Suspended,
                ..
            } if node.as_str() == "prod"
        )
    });
    assert!(suspended, "producer should have been suspended by backpressure");

    let resumed = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::PhaseTransition {
                node,
                from: NodePhase::Suspended,
                to: NodePhase::Running,
                ..
            } if node.as_str() == "prod"
        )
    });
    assert!(resumed, "producer should have been resumed after consumer drained");

    let faults: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fault { .. }))
        .collect();
    assert!(faults.is_empty(), "no faults expected: {faults:?}");
}

#[tokio::test]
async fn stream_close_completes_consumer() {
    let graph = Graph::builder("close-test")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(3)).unwrap()
        .connect(
            "s1",
            PortRef::new("prod", "items"),
            PortRef::new("cons", "items"),
        )
        .connect(
            "v1",
            PortRef::new("cons", "total"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &stream_wasm_map(),
        RuntimeConfig::default(),
        debug,
    )
    .await
    .expect("load");

    tick_until_done(&mut rt).await;

    assert_eq!(
        rt.node_state(&NodeId::from("prod")).unwrap().phase,
        NodePhase::Completed,
    );
    assert_eq!(
        rt.node_state(&NodeId::from("cons")).unwrap().phase,
        NodePhase::Completed,
    );
}

// ---- Custom host import (MQTT) test ----

#[tokio::test]
async fn custom_host_import_mqtt_feed() {
    let messages: Vec<Vec<u8>> = (0..5u32)
        .map(|i| val_to_bytes(&Val::U32(i)).unwrap())
        .collect();
    let feed: Arc<Mutex<VecDeque<Vec<u8>>>> =
        Arc::new(Mutex::new(VecDeque::from(messages)));

    let graph = Graph::builder("mqtt-test")
        .add_component(mqtt_node_contract())
        .add_component(u32_echo_contract())
        .add_node("mqtt", mqtt_node_cref())
        .add_node("sink", u32_echo_cref())
        .connect(
            "v1",
            PortRef::new("mqtt", "count"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let wasm = all_wasm_map();

    let feed_clone = feed.clone();
    let mut rt = RuntimeGraph::load_with_linker(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        witgraph_runtime::Release,
        move |linker| {
            let feed = feed_clone;
            let mut inst =
                linker.instance("witgraph:runtime/mqtt-source@0.1.0")?;
            inst.func_wrap(
                "next-message",
                move |_caller: wasmtime::StoreContextMut<'_, NodeHostState>, _params: ()| {
                    let msg = feed.lock().unwrap().pop_front();
                    Ok((msg,))
                },
            )?;
            Ok(())
        },
    )
    .await
    .expect("load with custom linker");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_u32_output(&rt, "mqtt", "count"), 5);
}

// ---- Wide graph test ----

#[tokio::test]
async fn wide_graph_full_flow() {
    let feed: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(
        (0..4u32).map(|i| val_to_bytes(&Val::U32(i)).unwrap()).collect(),
    ));

    let graph = Graph::builder("wide-graph")
        .add_component(echo_contract())
        .add_component(u32_echo_contract())
        .add_component(configurable_contract())
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(mqtt_node_contract())
        // Source layer
        .add_node("mqtt_src", mqtt_node_cref())
        .add_node("data_gen", stream_producer_cref())
        .add_node("cfg", configurable_cref())
        .set_config("data_gen", "burst-size", Val::U32(8)).unwrap()
        .set_config("cfg", "rate", Val::U32(4)).unwrap()
        .set_config("cfg", "gain", Val::F64(3.0)).unwrap()
        // Processing layer
        .add_node("consumer", stream_consumer_cref())
        .add_node("echo_fan", u32_echo_cref())   // u32: mqtt count
        .add_node("echo_a", u32_echo_cref())
        .add_node("echo_b", u32_echo_cref())
        .add_node("echo_cfg", echo_cref())        // f64: configurable result
        // Sink layer
        .add_node("sink_a", u32_echo_cref())
        .add_node("sink_b", u32_echo_cref())
        .add_node("sink_c", u32_echo_cref())      // u32: consumer total
        .add_node("sink_d", echo_cref())           // f64: configurable chain
        // mqtt_src (u32) → echo_fan → echo_a, echo_b → sinks
        .connect("m1", PortRef::new("mqtt_src", "count"), PortRef::new("echo_fan", "in"))
        .connect("f1", PortRef::new("echo_fan", "out"), PortRef::new("echo_a", "in"))
        .connect("f2", PortRef::new("echo_fan", "out"), PortRef::new("echo_b", "in"))
        .connect("a1", PortRef::new("echo_a", "out"), PortRef::new("sink_a", "in"))
        .connect("b1", PortRef::new("echo_b", "out"), PortRef::new("sink_b", "in"))
        // data_gen → consumer → sink_c (u32)
        .connect("s1", PortRef::new("data_gen", "items"), PortRef::new("consumer", "items"))
        .connect("c1", PortRef::new("consumer", "total"), PortRef::new("sink_c", "in"))
        // cfg (f64) → echo_cfg → sink_d
        .connect("v1", PortRef::new("cfg", "result"), PortRef::new("echo_cfg", "in"))
        .connect("d1", PortRef::new("echo_cfg", "out"), PortRef::new("sink_d", "in"))
        .build();

    let compiled = graph.compile().expect("valid wide graph");
    let wasm = all_wasm_map();
    let debug = Debug::new();

    let feed_clone = feed.clone();
    let mut rt = RuntimeGraph::load_with_linker(
        compiled,
        &wasm,
        RuntimeConfig {
            channel_capacity: 2,
            ..RuntimeConfig::default()
        },
        debug,
        move |linker| {
            let feed = feed_clone;
            let mut inst =
                linker.instance("witgraph:runtime/mqtt-source@0.1.0")?;
            inst.func_wrap(
                "next-message",
                move |_caller: wasmtime::StoreContextMut<'_, NodeHostState>, _params: ()| {
                    let msg = feed.lock().unwrap().pop_front();
                    Ok((msg,))
                },
            )?;
            Ok(())
        },
    )
    .await
    .expect("load wide graph");

    let ticks = tick_until_done(&mut rt).await;
    assert!(ticks < 50, "graph should settle within 50 ticks, took {ticks}");

    // Data correctness
    assert_eq!(read_u32_output(&rt, "consumer", "total"), 28, "sum(0..8)");
    assert_eq!(read_f64(&rt, "echo_cfg", "out"), 12.0, "4 * 3.0");

    // Stream nodes complete
    assert_eq!(
        rt.node_state(&NodeId::from("data_gen")).unwrap().phase,
        NodePhase::Completed,
    );
    assert_eq!(
        rt.node_state(&NodeId::from("consumer")).unwrap().phase,
        NodePhase::Completed,
    );

    // Trace assertions
    let trace = rt.mode().trace();

    let has_backpressure = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::PhaseTransition {
                node,
                to: NodePhase::Suspended,
                ..
            } if node.as_str() == "data_gen"
        )
    });
    assert!(
        has_backpressure,
        "stream producer should hit backpressure with capacity=2 and burst=8"
    );

    let faults: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fault { .. }))
        .collect();
    assert!(faults.is_empty(), "no faults in wide graph: {faults:?}");
}

use witgraph_ir::ResourceClaim;
use witgraph_runtime::NodeHostState;

// ---- Fuel / resource limit tests ----

#[tokio::test]
async fn fuel_exhaustion_faults_node() {
    let graph = Graph::builder("fuel-test")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("e", "in", Val::F64(1.0)).unwrap()
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig {
            fuel_per_activation: Some(1),
            ..RuntimeConfig::default()
        },
        debug,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(
        rt.node_state(&NodeId::from("e")).unwrap().phase,
        NodePhase::Faulted,
    );

    let trace = rt.mode().trace();
    let has_fuel_fault = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::Fault {
                fault: witgraph_runtime::NodeFault::FuelExhausted,
                ..
            }
        )
    });
    assert!(has_fuel_fault, "expected FuelExhausted fault in trace");
}

// ---- Cancellation tests ----

#[tokio::test]
async fn cancel_ready_node() {
    let graph = Graph::builder("cancel-test")
        .add_component(echo_contract())
        .add_node("e", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("e", "in", Val::F64(1.0)).unwrap()
        .connect("c1", PortRef::new("e", "out"), PortRef::new("sink", "in"))
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        debug,
    )
    .await
    .expect("load");

    rt.cancel(&NodeId::from("e")).expect("cancel");
    tick_until_idle(&mut rt).await;

    assert_eq!(
        rt.node_state(&NodeId::from("e")).unwrap().phase,
        NodePhase::Cancelled,
    );

    let trace = rt.mode().trace();
    let has_cancel = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::Cancelled { node } if node.as_str() == "e"
        )
    });
    assert!(has_cancel, "expected Cancelled trace event");
}

#[tokio::test]
async fn cancel_suspended_node() {
    let graph = Graph::builder("cancel-suspended-test")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(8)).unwrap()
        .connect(
            "s1",
            PortRef::new("prod", "items"),
            PortRef::new("cons", "items"),
        )
        .connect(
            "v1",
            PortRef::new("cons", "total"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &stream_wasm_map(),
        RuntimeConfig {
            channel_capacity: 2,
            max_steps_per_tick: 1,
            ..RuntimeConfig::default()
        },
        debug,
    )
    .await
    .expect("load");

    // One step: producer activates, commit hits full channel → suspended.
    let result = rt.tick().await;
    assert!(
        matches!(result, TickResult::StepLimitReached),
        "expected step limit, got {result:?}"
    );

    assert_eq!(
        rt.node_state(&NodeId::from("prod")).unwrap().phase,
        NodePhase::Suspended,
    );

    // Cancel the suspended producer.
    rt.cancel(&NodeId::from("prod")).expect("cancel");

    // Process cancellation.
    rt.tick().await;

    assert_eq!(
        rt.node_state(&NodeId::from("prod")).unwrap().phase,
        NodePhase::Cancelled,
    );
}

#[tokio::test]
async fn cancel_propagates_upstream() {
    let graph = Graph::builder("propagation-test")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(3)).unwrap()
        .connect(
            "s1",
            PortRef::new("prod", "items"),
            PortRef::new("cons", "items"),
        )
        .connect(
            "v1",
            PortRef::new("cons", "total"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &stream_wasm_map(),
        RuntimeConfig::default(),
        debug,
    )
    .await
    .expect("load");

    // Run to completion — consumer completes on stream close.
    tick_until_done(&mut rt).await;

    assert_eq!(
        rt.node_state(&NodeId::from("cons")).unwrap().phase,
        NodePhase::Completed,
    );
    // Producer also completed itself via ActivationResult::Completed.
    assert_eq!(
        rt.node_state(&NodeId::from("prod")).unwrap().phase,
        NodePhase::Completed,
    );

    // Now test upstream propagation with an externally-cancelled consumer.
    // Build a fresh graph where we cancel the consumer mid-stream.
    let graph2 = Graph::builder("propagation-test-2")
        .add_component(echo_contract())
        .add_component(u32_echo_contract())
        .add_node("src", echo_cref())
        .add_node("mid", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("src", "in", Val::F64(1.0)).unwrap()
        .connect(
            "c1",
            PortRef::new("src", "out"),
            PortRef::new("mid", "in"),
        )
        .connect(
            "c2",
            PortRef::new("mid", "out"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled2 = graph2.compile().expect("valid graph");
    let debug2 = Debug::new();
    let mut rt2 = RuntimeGraph::load(
        compiled2,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        debug2,
    )
    .await
    .expect("load");

    // Run to idle (all sync value nodes settle).
    tick_until_idle(&mut rt2).await;

    // Cancel the terminal node.
    rt2.cancel(&NodeId::from("sink")).expect("cancel");
    // Process cancellation — should propagate upstream through mid to src.
    tick_until_idle(&mut rt2).await;

    assert_eq!(
        rt2.node_state(&NodeId::from("sink")).unwrap().phase,
        NodePhase::Cancelled,
    );
    // mid's only consumer (sink) is terminal → mid should be cancelled.
    assert_eq!(
        rt2.node_state(&NodeId::from("mid")).unwrap().phase,
        NodePhase::Cancelled,
    );
    // src's only consumer (mid) is terminal → src should be cancelled.
    assert_eq!(
        rt2.node_state(&NodeId::from("src")).unwrap().phase,
        NodePhase::Cancelled,
    );
}

#[tokio::test]
async fn cancelled_node_restarts_on_input() {
    let graph = Graph::builder("restart-test")
        .add_component(echo_contract())
        .add_node("src", echo_cref())
        .add_node("dst", echo_cref())
        .add_node("sink", echo_cref())
        .set_config("src", "in", Val::F64(1.0)).unwrap()
        .connect(
            "c1",
            PortRef::new("src", "out"),
            PortRef::new("dst", "in"),
        )
        .connect(
            "c2",
            PortRef::new("dst", "out"),
            PortRef::new("sink", "in"),
        )
        .build();
    let compiled = graph.compile().expect("valid graph");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        debug,
    )
    .await
    .expect("load");

    // Initial run.
    tick_until_idle(&mut rt).await;
    assert_eq!(read_f64(&rt, "dst", "out"), 1.0);

    // Cancel the entire chain.
    rt.cancel(&NodeId::from("sink")).expect("cancel");
    tick_until_idle(&mut rt).await;
    assert_eq!(
        rt.node_state(&NodeId::from("src")).unwrap().phase,
        NodePhase::Cancelled,
    );
    assert_eq!(
        rt.node_state(&NodeId::from("dst")).unwrap().phase,
        NodePhase::Cancelled,
    );

    // Inject new input → src should restart, propagate to dst and sink.
    rt.inject(NodeId::from("src"), PortName::from("in"), Val::F64(99.0))
        .expect("inject");

    // Need multiple ticks: restart src → activate src → restart dst →
    // activate dst → restart sink → activate sink.
    for _ in 0..10 {
        match rt.tick().await {
            TickResult::Idle | TickResult::Completed => break,
            TickResult::Progress => continue,
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert_eq!(read_f64(&rt, "dst", "out"), 99.0);

    let trace = rt.mode().trace();
    let restarts: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Restarted { .. }))
        .collect();
    assert!(
        !restarts.is_empty(),
        "expected at least one Restarted trace event"
    );
}

// ---- Resource budget tests ----

/// Two independent source nodes sharing a resource (0.6 each, sum > 1.0)
/// and one unconstrained node. All three should eventually produce output.
/// The unconstrained node is never deferred.
#[tokio::test]
async fn resource_budget_mutual_exclusion_with_unconstrained() {
    let graph = Graph::builder("resource-mixed")
        .add_component(echo_contract())
        // Two nodes sharing "disk" at 0.6 each (can't run together)
        .add_node("a", echo_cref())
        .add_node("b", echo_cref())
        // One unconstrained node (no resource claims)
        .add_node("free", echo_cref())
        // Sinks so we can read outputs
        .add_node("sink_a", echo_cref())
        .add_node("sink_b", echo_cref())
        .add_node("sink_free", echo_cref())
        .set_config("a", "in", Val::F64(1.0)).unwrap()
        .set_config("b", "in", Val::F64(2.0)).unwrap()
        .set_config("free", "in", Val::F64(3.0)).unwrap()
        .set_resource("a", "disk", ResourceClaim::new(0.6).unwrap()).unwrap()
        .set_resource("b", "disk", ResourceClaim::new(0.6).unwrap()).unwrap()
        .connect("ca", PortRef::new("a", "out"), PortRef::new("sink_a", "in"))
        .connect("cb", PortRef::new("b", "out"), PortRef::new("sink_b", "in"))
        .connect("cf", PortRef::new("free", "out"), PortRef::new("sink_free", "in"))
        .build();

    let compiled = graph.compile().expect("valid");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        debug,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "a", "out"), 1.0, "node a produced output");
    assert_eq!(read_f64(&rt, "b", "out"), 2.0, "node b produced output");
    assert_eq!(read_f64(&rt, "free", "out"), 3.0, "unconstrained node produced output");

    let trace = rt.mode().trace();
    let faults: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fault { .. }))
        .collect();
    assert!(faults.is_empty(), "no faults: {faults:?}");
}

/// Two nodes with compatible resource claims (0.5 + 0.5 = 1.0) should
/// both activate without deferral.
#[tokio::test]
async fn resource_budget_compatible_claims() {
    let graph = Graph::builder("resource-compatible")
        .add_component(echo_contract())
        .add_node("a", echo_cref())
        .add_node("b", echo_cref())
        .add_node("sink_a", echo_cref())
        .add_node("sink_b", echo_cref())
        .set_config("a", "in", Val::F64(10.0)).unwrap()
        .set_config("b", "in", Val::F64(20.0)).unwrap()
        .set_resource("a", "disk", ResourceClaim::new(0.5).unwrap()).unwrap()
        .set_resource("b", "disk", ResourceClaim::new(0.5).unwrap()).unwrap()
        .connect("ca", PortRef::new("a", "out"), PortRef::new("sink_a", "in"))
        .connect("cb", PortRef::new("b", "out"), PortRef::new("sink_b", "in"))
        .build();

    let compiled = graph.compile().expect("valid");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "a", "out"), 10.0);
    assert_eq!(read_f64(&rt, "b", "out"), 20.0);
}

/// Multiple named resources: nodes that fit on one resource but conflict
/// on another are correctly deferred.
#[tokio::test]
async fn resource_budget_multiple_resources() {
    let graph = Graph::builder("multi-resource")
        .add_component(echo_contract())
        .add_node("a", echo_cref())
        .add_node("b", echo_cref())
        .add_node("sink_a", echo_cref())
        .add_node("sink_b", echo_cref())
        .set_config("a", "in", Val::F64(1.0)).unwrap()
        .set_config("b", "in", Val::F64(2.0)).unwrap()
        // a: disk=0.6, gpu=0.3
        // b: disk=0.3, gpu=0.8
        // disk: 0.6+0.3=0.9 OK; gpu: 0.3+0.8=1.1 > 1.0 → conflict
        .set_resource("a", "disk", ResourceClaim::new(0.6).unwrap()).unwrap()
        .set_resource("a", "gpu", ResourceClaim::new(0.3).unwrap()).unwrap()
        .set_resource("b", "disk", ResourceClaim::new(0.3).unwrap()).unwrap()
        .set_resource("b", "gpu", ResourceClaim::new(0.8).unwrap()).unwrap()
        .connect("ca", PortRef::new("a", "out"), PortRef::new("sink_a", "in"))
        .connect("cb", PortRef::new("b", "out"), PortRef::new("sink_b", "in"))
        .build();

    let compiled = graph.compile().expect("valid");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "a", "out"), 1.0);
    assert_eq!(read_f64(&rt, "b", "out"), 2.0);
}

/// A pipeline where source → sink, source has a resource budget,
/// sink does not. Both should complete normally.
#[tokio::test]
async fn resource_budget_pipeline_with_unconstrained_sink() {
    let graph = Graph::builder("pipeline-mixed")
        .add_component(echo_contract())
        .add_node("src", echo_cref())
        .add_node("mid", echo_cref())
        .add_node("sink", echo_cref())
        .add_node("tail", echo_cref())
        .set_config("src", "in", Val::F64(42.0)).unwrap()
        .set_resource("src", "disk", ResourceClaim::new(1.0).unwrap()).unwrap()
        .connect("c1", PortRef::new("src", "out"), PortRef::new("mid", "in"))
        .connect("c2", PortRef::new("mid", "out"), PortRef::new("sink", "in"))
        .connect("c3", PortRef::new("sink", "out"), PortRef::new("tail", "in"))
        .build();

    let compiled = graph.compile().expect("valid");
    let mut rt = RuntimeGraph::load(
        compiled,
        &echo_wasm_map(),
        RuntimeConfig::default(),
        witgraph_runtime::Release,
    )
    .await
    .expect("load");

    tick_until_idle(&mut rt).await;

    assert_eq!(read_f64(&rt, "sink", "out"), 42.0);
}

/// Stream producer with resource budget → consumer without budget.
/// Exercises resource release across backpressure cycles.
#[tokio::test]
async fn resource_budget_with_backpressure() {
    let graph = Graph::builder("resource-bp")
        .add_component(stream_producer_contract())
        .add_component(stream_consumer_contract())
        .add_component(u32_echo_contract())
        .add_node("prod", stream_producer_cref())
        .add_node("cons", stream_consumer_cref())
        .add_node("sink", u32_echo_cref())
        .set_config("prod", "burst-size", Val::U32(5)).unwrap()
        .set_resource("prod", "disk", ResourceClaim::new(0.8).unwrap()).unwrap()
        .connect("s1", PortRef::new("prod", "items"), PortRef::new("cons", "items"))
        .connect("s2", PortRef::new("cons", "total"), PortRef::new("sink", "in"))
        .build();

    let compiled = graph.compile().expect("valid");
    let debug = Debug::new();
    let mut rt = RuntimeGraph::load(
        compiled,
        &all_wasm_map(),
        RuntimeConfig {
            channel_capacity: 2,
            ..RuntimeConfig::default()
        },
        debug,
    )
    .await
    .expect("load");

    let ticks = tick_until_done(&mut rt).await;
    assert!(ticks < 50, "settled within 50 ticks, took {ticks}");

    assert_eq!(read_u32_output(&rt, "cons", "total"), 10, "sum(0..5)");

    let trace = rt.mode().trace();
    let has_bp = trace.iter().any(|e| {
        matches!(
            e,
            TraceEvent::PhaseTransition {
                node,
                to: NodePhase::Suspended,
                ..
            } if node.as_str() == "prod"
        )
    });
    assert!(has_bp, "producer should hit backpressure with capacity=2 and burst=5");

    let faults: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Fault { .. }))
        .collect();
    assert!(faults.is_empty(), "no faults: {faults:?}");
}
