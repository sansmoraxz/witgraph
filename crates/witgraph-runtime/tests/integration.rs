#![cfg(test)]
#![allow(missing_docs)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use witgraph_runtime::RunningGraph;

use test_components::{
    BLOB_SINK, BLOB_SOURCE, BUSY_LOOP, BYTES_CONSUMER, CALC, CONFIGURABLE, ECHO, EDGE_DETECT,
    FRAME_GRAYSCALE, FRAME_INVERT, FUTURE_WRITER, Guest, LABELLED, MATH, MAYBE, MQTT_NODE,
    NAMED_ECHO, READING_CONSUMER, READING_PRODUCER, RELAY, RELAY_PLUS, RELAY_WIDE, STREAM_CONSUMER,
    STREAM_DRAIN, STREAM_PRODUCER, STREAM_RELAY, VIDEO_SINK, VIDEO_SOURCE,
};
use wasmtime::StoreContextMut;
use wasmtime::component::{ComponentType, Lift, Lower, Resource, ResourceType};
use witgraph_ir::{
    Capability, ComponentContract, ComponentRef, Graph, GraphBuilder, NodeId, PortDirection,
    PortRef, ResourceClaim,
};
use witgraph_runtime::{
    CapabilityPlugin, Host, HostState, IslandSnapshot, LinkedProvider, LoadError, NodeFault,
    NodePhase, PluginData, Plugins, PreparedComponent, RuntimeConfig, RuntimeError, RuntimeGraph,
    RuntimeMode, Snapshot, TickResult, Trace, TraceEvent, Val,
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

/// Provides `test:mqtt/source` to the nodes that import it, all reading
/// one shared feed of byte messages.
struct Mqtt {
    feed: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl Host for Mqtt {
    type Data = HostState;

    fn link(
        &self,
        _node: &NodeId,
        contract: &ComponentContract,
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> wasmtime::Result<()> {
        const SOURCE: &str = "test:mqtt/source@0.1.0";
        if !contract.capabilities.iter().any(|c| c.interface == SOURCE) {
            return Ok(());
        }
        let feed = self.feed.clone();
        linker.instance(SOURCE)?.func_wrap(
            "next-message",
            move |_store: StoreContextMut<'_, HostState>, (): ()| {
                Ok((feed.lock().unwrap().pop_front(),))
            },
        )?;
        Ok(())
    }

    fn island_data(&self, _members: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }
}

fn mqtt(feed: Arc<Mutex<VecDeque<Vec<u8>>>>) -> Mqtt {
    Mqtt { feed }
}

fn id(node: &str) -> NodeId {
    NodeId::from(node)
}

fn port(node: &str, port: &str) -> PortRef {
    PortRef::new(node, port)
}

async fn read<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
) -> Option<Val> {
    rt.read_output(&id(node), &port.into())
        .await
        .expect("a Value output")
}

async fn read_f64<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
) -> f64 {
    match read(rt, node, port).await {
        Some(Val::Float64(v)) => v,
        other => panic!("{node}.{port}: expected f64, got {other:?}"),
    }
}

async fn read_u32<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
) -> u32 {
    match read(rt, node, port).await {
        Some(Val::U32(v)) => v,
        other => panic!("{node}.{port}: expected u32, got {other:?}"),
    }
}

async fn inject<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
    val: Val,
) {
    rt.inject(&id(node), &port.into(), val)
        .await
        .expect("inject");
}

fn phase<M: RuntimeMode, H: Host>(rt: &RuntimeGraph<M, H>, node: &str) -> NodePhase {
    rt.node_state(&id(node)).expect("node exists").phase()
}

/// Ticks until the graph is idle. Every tick must finish within 30s.
async fn settle<M: RuntimeMode, H: Host>(rt: &mut RuntimeGraph<M, H>) -> usize {
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(2.5)).await;
    settle(&mut rt).await;

    assert_eq!(read_f64(&mut rt, "a", "out").await, 2.5);
    assert_eq!(
        read_f64(&mut rt, "b", "out").await,
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "cfg", "rate", Val::U32(4)).await;
    inject(&mut rt, "cfg", "gain", Val::Float64(3.0)).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "cfg", "result").await, 12.0);
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "c", "out").await, 1.0);
    assert_eq!(
        runs_of(&rt.mode().trace(), "c"),
        1,
        "c waits for its ancestors"
    );

    rt.inject(&id("a"), &"in".into(), Val::Float64(2.0))
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "c", "out").await, 2.0);
    assert_eq!(runs_of(&rt.mode().trace(), "c"), 2);

    // Writing the same value again is not a change.
    rt.inject(&id("a"), &"in".into(), Val::Float64(2.0))
        .await
        .unwrap();
    assert!(matches!(rt.tick().await, TickResult::Idle));
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 2);
}

#[tokio::test]
async fn inject_rejects_bad_targets_and_types() {
    let kit = Kit::new(&[ECHO, STREAM_CONSUMER, STREAM_PRODUCER]);
    let graph = kit
        .builder("inject")
        .add_node("e", kit.get(ECHO))
        .add_node("f", kit.get(ECHO))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("ef", port("e", "out"), port("f", "in"))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;

    let err = rt
        .inject(&id("e"), &"in".into(), Val::U32(1))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::ValueType { .. }), "{err}");
    let err = rt
        .inject(&id("cons"), &"items".into(), Val::U32(1))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotAValuePort { .. }), "{err}");
    let err = rt
        .inject(&id("ghost"), &"in".into(), Val::Float64(1.0))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::UnknownNode { .. }), "{err}");
    let err = rt
        .inject(&id("f"), &"in".into(), Val::Float64(1.0))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::ConnectedInput { ref connection, .. } if connection.as_str() == "ef"),
        "an input has one writer: {err}"
    );
}

#[tokio::test]
async fn inject_rejects_an_input_fed_inside_its_island() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let graph = kit
        .builder("in-island")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("l", port("prod", "limit"), port("cons", "take"))
        .build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    let err = rt
        .inject(&id("cons"), &"take".into(), Val::U32(2))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::ConnectedInput { .. }),
        "the producer's value would overwrite it: {err}"
    );
}

use witgraph_runtime::Perf;

#[tokio::test]
async fn an_option_output_feeds_an_optional_input_straight_through() {
    let kit = Kit::new(&[MAYBE, STREAM_PRODUCER, STREAM_CONSUMER]);
    let graph = kit
        .builder("maybe")
        .add_node("m", kit.get(MAYBE))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("take", port("m", "maybe"), port("cons", "take"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "prod", "burst-size", Val::U32(10)).await;
    inject(&mut rt, "m", "x", Val::U32(3)).await;
    settle(&mut rt).await;
    assert_eq!(
        read_u32(&mut rt, "cons", "count").await,
        3,
        "`some(3)` arrives as 3"
    );
    rt.clear_input(&id("m"), &"x".into()).await.unwrap();
    settle(&mut rt).await;
    assert_eq!(
        read_u32(&mut rt, "cons", "count").await,
        10,
        "`none` reads as an absent `take`"
    );
}

#[tokio::test]
async fn zero_limits_are_invalid() {
    let kit = Kit::new(&[ECHO]);
    for config in [
        RuntimeConfig {
            max_steps_per_tick: 0,
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            yield_interval: 0,
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            fuel_per_run: Some(0),
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            max_island_memory: Some(0),
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            hostcall_fuel: 0,
            ..RuntimeConfig::default()
        },
        RuntimeConfig {
            memory_reservation: Some(u64::MAX),
            ..RuntimeConfig::default()
        },
    ] {
        let graph = kit.builder("zero").add_node("e", kit.get(ECHO)).build();
        let compiled = graph.compile(&kit.contracts).expect("compiles");
        let err = RuntimeGraph::load(compiled, &kit.wasm, config, Perf)
            .await
            .err()
            .expect("the limit is rejected");
        assert!(
            matches!(err, LoadError::Runtime(RuntimeError::InvalidConfig { .. })),
            "{err}"
        );
    }
}

#[tokio::test]
async fn a_zero_memory_reservation_is_valid() {
    let kit = Kit::new(&[ECHO]);
    let config = RuntimeConfig {
        memory_reservation: Some(0),
        ..RuntimeConfig::default()
    };
    let graph = kit.builder("zero").add_node("e", kit.get(ECHO)).build();
    let mut rt = kit.load(graph, config, Perf).await;
    assert!(rt.config().engine.is_some(), "the config holds the engine");
    inject(&mut rt, "e", "in", Val::Float64(3.0)).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "e", "out").await, 3.0);
}

// ---- Several implementations and contracts in one graph ----

#[tokio::test]
async fn two_versions_of_one_contract_run_side_by_side() {
    let kit = Kit::new(&[RELAY, RELAY_PLUS]);
    let (v1, v2) = (kit.get(RELAY), kit.get(RELAY_PLUS));
    assert_eq!(
        v1.content_hash, v2.content_hash,
        "the same contract: the version is not part of the hash"
    );
    assert_ne!(v1, v2, "but two components");
    let graph = kit
        .builder("versions")
        .add_node("a", v1)
        .add_node("b", v2)
        .connect("ab", port("a", "out"), port("b", "in"))
        // `a.in` is required: close the loop, and seed it.
        .connect_feedback("ba", port("b", "out"), port("a", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::U32(10)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    inject(&mut rt, "b", "add", Val::U32(1)).await;
    assert!(
        matches!(rt.tick().await, TickResult::Progress),
        "one iteration"
    );
    assert_eq!(read_u32(&mut rt, "a", "out").await, 11, "0.1.0: in + add");
    assert_eq!(
        read_u32(&mut rt, "b", "out").await,
        13,
        "0.2.0: in + add + 1"
    );
    let snapshot = rt.snapshot().await.unwrap();
    let minor = |node: &str| {
        snapshot.nodes[&id(node)]
            .package
            .version
            .as_ref()
            .unwrap()
            .minor
    };
    assert_eq!((minor("a"), minor("b")), (1, 2));
}

#[tokio::test]
async fn two_revisions_of_one_world_are_told_apart_by_hash() {
    let kit = Kit::new(&[RELAY, RELAY_WIDE]);
    let (narrow, wide) = (kit.get(RELAY), kit.get(RELAY_WIDE));
    assert_eq!(
        (&narrow.package, &narrow.world),
        (&wide.package, &wide.world),
        "one world id"
    );
    assert_ne!(narrow.content_hash, wide.content_hash, "two contracts");

    let graph = kit
        .builder("revisions")
        .add_node("n", narrow.clone())
        .add_node("w", wide.clone())
        .connect("nw", port("n", "out"), port("w", "in"))
        // `n.in` is required: close the loop, and seed it.
        .connect_feedback("wn", port("w", "out"), port("n", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "n", "in", Val::U32(3)).await;
    inject(&mut rt, "n", "add", Val::U32(2)).await;
    assert!(
        matches!(rt.tick().await, TickResult::Progress),
        "one iteration"
    );
    assert_eq!(read_u32(&mut rt, "n", "out").await, 5);
    assert_eq!(read_u32(&mut rt, "w", "out").await, 5);
    assert_eq!(
        read_u32(&mut rt, "w", "doubled").await,
        10,
        "the wide revision ran"
    );
    assert!(
        matches!(
            rt.read_output(&id("n"), &"doubled".into()).await,
            Err(RuntimeError::NotAValuePort { .. })
        ),
        "the narrow one has no such port"
    );

    // Unpinned, the reference matches both revisions.
    let mut unpinned = narrow.clone();
    unpinned.content_hash = None;
    let ambiguous = kit
        .builder("ambiguous")
        .add_node("x", unpinned)
        .build()
        .compile(&kit.contracts)
        .unwrap_err();
    assert!(
        ambiguous
            .diagnostics
            .iter()
            .any(|d| matches!(d, witgraph_ir::Diagnostic::AmbiguousComponent { .. })),
        "{}",
        ambiguous.diagnostics
    );

    // The narrow bytes under the wide revision's key do not implement it.
    let graph = kit
        .builder("swapped")
        .add_node("w", wide.clone())
        .connect_feedback("ww", port("w", "out"), port("w", "in"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let wasm = HashMap::from([(wide, RELAY.wasm), (narrow, RELAY.wasm)]);
    let err = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf)
        .await
        .err()
        .expect("relay's bytes lack `doubled`");
    assert!(matches!(err, LoadError::ContractMismatch { .. }), "{err}");
}

#[tokio::test]
async fn named_and_inline_node_contracts_mix_in_one_graph() {
    let kit = Kit::new(&[ECHO, NAMED_ECHO, RELAY]);
    let graph = kit
        .builder("styles")
        .add_node("e", kit.get(ECHO))
        .add_node("n", kit.get(NAMED_ECHO))
        .add_node("f", kit.get(ECHO))
        .connect("en", port("e", "out"), port("n", "in"))
        .connect("nf", port("n", "out"), port("f", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "e", "in", Val::Float64(1.5)).await;
    settle(&mut rt).await;
    assert_eq!(
        read_f64(&mut rt, "n", "out").await,
        3.0,
        "the named-interface node ran"
    );
    assert_eq!(read_f64(&mut rt, "f", "out").await, 3.0);

    // Different contracts connect only where their port types agree.
    let mismatched = kit
        .builder("mismatch")
        .add_node("r", kit.get(RELAY))
        .add_node("n", kit.get(NAMED_ECHO))
        .connect("rn", port("r", "out"), port("n", "in"))
        .build()
        .compile(&kit.contracts)
        .unwrap_err();
    assert!(
        mismatched
            .diagnostics
            .iter()
            .any(|d| matches!(d, witgraph_ir::Diagnostic::TypeMismatch { .. })),
        "u32 -> f64: {}",
        mismatched.diagnostics
    );
}

// ---- Streams ----

fn pipeline(kit: &Kit) -> Graph {
    kit.builder("pipeline")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build()
}

/// Both island layouts: one Store per island (the default), and members
/// joined by a pumpable stream in Stores of their own.
fn both_layouts() -> [RuntimeConfig; 2] {
    [RuntimeConfig::default(), split()]
}

fn split() -> RuntimeConfig {
    RuntimeConfig {
        split_islands: true,
        ..RuntimeConfig::default()
    }
}

/// Sets the producer's burst size and the consumer's take count; `None`
/// leaves the optional input absent (endless producer / take everything).
async fn feed_pipeline<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    burst: Option<u32>,
    take: Option<u32>,
) {
    if let Some(burst) = burst {
        inject(rt, "prod", "burst-size", Val::U32(burst)).await;
    }
    if let Some(take) = take {
        inject(rt, "cons", "take", Val::U32(take)).await;
    }
}

#[tokio::test]
async fn stream_producer_consumer_share_an_island() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    for config in both_layouts() {
        let split = config.split_islands;
        let mut rt = kit.load(pipeline(&kit), config, Trace::new()).await;
        feed_pipeline(&mut rt, Some(5), None).await;
        assert_eq!(rt.compiled().islands().len(), 1);
        settle(&mut rt).await;

        assert_eq!(
            read_u32(&mut rt, "cons", "total").await,
            10,
            "split: {split}"
        );
        assert_eq!(
            read_u32(&mut rt, "cons", "count").await,
            5,
            "split: {split}"
        );
        assert_eq!(phase(&rt, "prod"), NodePhase::Idle, "split: {split}");
        assert_eq!(phase(&rt, "cons"), NodePhase::Idle, "split: {split}");
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
}

#[tokio::test]
async fn backpressure_slow_consumer_gets_every_item() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    for config in both_layouts() {
        let split = config.split_islands;
        let mut rt = kit.load(pipeline(&kit), config, Trace::new()).await;
        feed_pipeline(&mut rt, Some(2000), None).await;
        inject(&mut rt, "cons", "delay", Val::U32(3)).await;
        settle(&mut rt).await;
        assert_eq!(
            read_u32(&mut rt, "cons", "count").await,
            2000,
            "split: {split}"
        );
        let total = (0..2000).sum::<u32>();
        assert_eq!(
            read_u32(&mut rt, "cons", "total").await,
            total,
            "split: {split}"
        );
    }
}

#[tokio::test]
async fn early_reader_drop_stops_an_infinite_producer() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    for config in both_layouts() {
        let split = config.split_islands;
        // No burst-size: the producer streams forever unless its reader
        // goes.
        let mut rt = kit.load(pipeline(&kit), config, Trace::new()).await;
        feed_pipeline(&mut rt, None, Some(3)).await;
        settle(&mut rt).await;
        assert_eq!(
            read_u32(&mut rt, "cons", "count").await,
            3,
            "split: {split}"
        );
        assert_eq!(
            read_u32(&mut rt, "cons", "total").await,
            3,
            "split: {split}"
        );
        // The generation finished, so the producer's writer task exited.
        assert_eq!(phase(&rt, "prod"), NodePhase::Idle, "split: {split}");
    }
}

/// prod → relay → cons, where the relay returns the stream it was given:
/// split, the relay's Store forwards a stream the host writes.
fn relayed(kit: &Kit) -> Graph {
    kit.builder("relayed")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("relay", kit.get(STREAM_RELAY))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("a", port("prod", "items"), port("relay", "items"))
        .connect("b", port("relay", "items"), port("cons", "items"))
        .build()
}

#[tokio::test]
async fn a_stream_passed_through_a_node_reaches_its_reader() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_RELAY, STREAM_CONSUMER]);
    for config in both_layouts() {
        let split = config.split_islands;
        let mut rt = kit.load(relayed(&kit), config, Trace::new()).await;
        // More items than a pump holds at once, so the relay's Store keeps
        // forwarding after the relay returned.
        feed_pipeline(&mut rt, Some(100_000), None).await;
        settle(&mut rt).await;
        assert_eq!(
            read_u32(&mut rt, "cons", "count").await,
            100_000,
            "split: {split}"
        );
        let total = (0..100_000u32).fold(0u32, u32::wrapping_add);
        assert_eq!(
            read_u32(&mut rt, "cons", "total").await,
            total,
            "split: {split}"
        );
        for node in ["prod", "relay", "cons"] {
            assert_eq!(phase(&rt, node), NodePhase::Idle, "{node}, split: {split}");
        }
    }
}

#[tokio::test]
async fn a_passed_through_stream_is_forwarded_after_its_reader_returned() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_RELAY, STREAM_DRAIN]);
    let graph = kit
        .builder("drained")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("relay", kit.get(STREAM_RELAY))
        .add_node("drain", kit.get(STREAM_DRAIN))
        .connect("a", port("prod", "items"), port("relay", "items"))
        .connect("b", port("relay", "items"), port("drain", "items"))
        .build();
    for config in both_layouts() {
        let split = config.split_islands;
        let mut rt = kit.load(graph.clone(), config, Trace::new()).await;
        inject(&mut rt, "prod", "burst-size", Val::U32(100_000)).await;
        // Every call returns before the stream ends: the generation lasts
        // until the producer has written it all and the drain read it.
        settle(&mut rt).await;
        for node in ["prod", "relay", "drain"] {
            assert_eq!(phase(&rt, node), NodePhase::Idle, "{node}, split: {split}");
        }
    }
}

#[tokio::test]
async fn a_reader_dropping_a_passed_through_stream_stops_its_producer() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_RELAY, STREAM_CONSUMER]);
    for config in both_layouts() {
        let split = config.split_islands;
        for _ in 0..10 {
            let mut rt = kit.load(relayed(&kit), config.clone(), Trace::new()).await;
            feed_pipeline(&mut rt, None, Some(3)).await;
            settle(&mut rt).await;
            assert_eq!(
                read_u32(&mut rt, "cons", "count").await,
                3,
                "split: {split}"
            );
            // The generation finished, so the producer's writer task exited.
            assert_eq!(phase(&rt, "prod"), NodePhase::Idle, "split: {split}");
        }
    }
}

#[tokio::test]
async fn a_future_output_nothing_reads_is_closed() {
    let kit = Kit::new(&[FUTURE_WRITER]);
    let graph = kit
        .builder("future")
        .add_node("w", kit.get(FUTURE_WRITER))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    settle(&mut rt).await;
    inject(&mut rt, "w", "again", Val::U32(1)).await;
    settle(&mut rt).await;
    // The first run's write found the read end dropped.
    assert_eq!(read_u32(&mut rt, "w", "last").await, 2);
    assert!(faults(&rt.mode().trace()).is_empty());
}

#[tokio::test]
async fn capability_import_feeds_a_stream() {
    let kit = Kit::new(&[MQTT_NODE, BYTES_CONSUMER]);
    let graph = kit
        .builder("mqtt")
        .add_node("mqtt", kit.get(MQTT_NODE))
        .add_node("cons", kit.get(BYTES_CONSUMER))
        .connect("s", port("mqtt", "messages"), port("cons", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    assert_eq!(compiled.required_capabilities().len(), 1);
    let feed = Arc::new(Mutex::new(VecDeque::from([
        vec![3u8, 4],
        vec![5],
        vec![6, 7, 8],
        vec![9],
        vec![10],
    ])));
    let mut rt = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Perf,
        mqtt(feed.clone()),
    )
    .await
    .expect("loads with the capability");
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "count").await, 5);
    assert_eq!(read_u32(&mut rt, "cons", "total-bytes").await, 8);
    assert!(feed.lock().unwrap().is_empty());
}

/// Provides `wasi:clocks/monotonic-clock` to the nodes that import it:
/// `wait-for` waits on tokio's clock, from a concurrent host function (the import is an
/// `async func`).
struct Clock;

impl Host for Clock {
    type Data = HostState;

    fn link(
        &self,
        _node: &NodeId,
        contract: &ComponentContract,
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> wasmtime::Result<()> {
        const CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.2";
        if !contract.capabilities.iter().any(|c| c.interface == CLOCK) {
            return Ok(());
        }
        linker
            .instance(CLOCK)?
            .func_wrap_concurrent("wait-for", |_, (nanos,): (u64,)| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_nanos(nanos)).await;
                    Ok(())
                })
            })
    }

    fn island_data(&self, _members: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }
}

/// `source` streaming `frames` frames at `fps`, through `filters` in
/// order, into `sink`.
async fn video(
    kit: &Kit,
    filters: &[(&str, Guest)],
    frames: u32,
    fps: Option<u32>,
) -> RuntimeGraph<Perf, Clock> {
    let mut builder = kit
        .builder("video")
        .add_node("source", kit.get(VIDEO_SOURCE))
        .add_node("sink", kit.get(VIDEO_SINK));
    let mut from = "source";
    for (node, guest) in filters {
        builder = builder.add_node(*node, kit.get(*guest)).connect(
            format!("{from}-{node}"),
            port(from, "frames"),
            port(node, "frames"),
        );
        from = node;
    }
    let graph = builder
        .connect("to-sink", port(from, "frames"), port("sink", "frames"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let mut rt =
        RuntimeGraph::load_with_host(compiled, &kit.wasm, RuntimeConfig::default(), Perf, Clock)
            .await
            .expect("loads with the clock");
    inject(&mut rt, "source", "frames", Val::U32(frames)).await;
    if let Some(fps) = fps {
        inject(&mut rt, "source", "fps", Val::U32(fps)).await;
    }
    rt
}

async fn read_bool<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
) -> bool {
    match read(rt, node, port).await {
        Some(Val::Bool(v)) => v,
        other => panic!("{node}.{port}: expected bool, got {other:?}"),
    }
}

#[tokio::test]
async fn a_video_pipeline_filters_frames_as_they_stream() {
    let kit = Kit::new(&[VIDEO_SOURCE, FRAME_GRAYSCALE, FRAME_INVERT, VIDEO_SINK]);

    // Paced by the host's clock.
    let filters = [("gray", FRAME_GRAYSCALE), ("invert", FRAME_INVERT)];
    let mut rt = video(&kit, &filters, 5, Some(200)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "sink", "count").await, 5);
    assert!(read_bool(&mut rt, "sink", "gray").await);
    // The last frame's first pixel is hue 24 (255, 102, 0): luma 136,
    // inverted 119.
    assert_eq!(read_u32(&mut rt, "sink", "first-pixel").await, 0x7777_77ff);

    // Unpaced and unfiltered, the frames are in colour.
    let mut rt = video(&kit, &[], 3, None).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "sink", "count").await, 3);
    assert!(!read_bool(&mut rt, "sink", "gray").await);
}

/// `test:accel/cnn`'s types, as the host sees them.
mod cnn {
    use super::{ComponentType, Lift, Lower};

    #[derive(ComponentType, Lift, Lower, Clone, Debug)]
    #[component(record)]
    pub struct Tensor {
        pub channels: u32,
        pub height: u32,
        pub width: u32,
        pub data: Vec<f32>,
    }

    #[derive(ComponentType, Lift, Lower, Clone, Debug)]
    #[component(record)]
    pub struct Conv {
        #[component(name = "out-channels")]
        pub out_channels: u32,
        pub size: u32,
        pub weights: Vec<f32>,
        pub bias: Vec<f32>,
    }

    #[derive(ComponentType, Lift, Lower, Clone, Debug)]
    #[component(variant)]
    pub enum Layer {
        #[component(name = "conv")]
        Conv(Conv),
        #[component(name = "relu")]
        Relu,
        #[component(name = "clamp")]
        Clamp((f32, f32)),
    }

    /// `layers` over `input` on the CPU: the reference the browsers'
    /// accelerators are held to.
    pub fn run(layers: &[Layer], input: Tensor) -> Result<Tensor, String> {
        let mut t = input;
        for layer in layers {
            match layer {
                Layer::Conv(conv) => {
                    let (c, h, w) = (t.channels as usize, t.height as usize, t.width as usize);
                    let (o, k) = (conv.out_channels as usize, conv.size as usize);
                    if conv.weights.len() != o * c * k * k || conv.bias.len() != o {
                        return Err("the convolution's weights do not fit its input".into());
                    }
                    let r = (k / 2) as isize;
                    let at = |ic: usize, y: isize, x: isize| {
                        let inside = (0..h as isize).contains(&y) && (0..w as isize).contains(&x);
                        if inside {
                            t.data[(ic * h + y as usize) * w + x as usize]
                        } else {
                            0.0
                        }
                    };
                    let mut out = vec![0.0; o * h * w];
                    for oc in 0..o {
                        for y in 0..h {
                            for x in 0..w {
                                let mut sum = conv.bias[oc];
                                for ic in 0..c {
                                    for ky in 0..k {
                                        for kx in 0..k {
                                            let v = at(
                                                ic,
                                                y as isize + ky as isize - r,
                                                x as isize + kx as isize - r,
                                            );
                                            sum +=
                                                v * conv.weights[((oc * c + ic) * k + ky) * k + kx];
                                        }
                                    }
                                }
                                out[(oc * h + y) * w + x] = sum;
                            }
                        }
                    }
                    t = Tensor {
                        channels: conv.out_channels,
                        data: out,
                        ..t
                    };
                }
                Layer::Relu => t.data.iter_mut().for_each(|v| *v = v.max(0.0)),
                Layer::Clamp((lo, hi)) => t.data.iter_mut().for_each(|v| *v = v.clamp(*lo, *hi)),
            }
        }
        Ok(t)
    }
}

/// The clock, and `test:accel/cnn` on the CPU; or, when `accelerated` is
/// false, no accelerator (`backend` says why).
struct Accel {
    accelerated: bool,
}

impl Host for Accel {
    type Data = HostState;

    fn link(
        &self,
        node: &NodeId,
        contract: &ComponentContract,
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> wasmtime::Result<()> {
        Clock.link(node, contract, linker)?;
        const CNN: &str = "test:accel/cnn@0.1.0";
        if !contract.capabilities.iter().any(|c| c.interface == CNN) {
            return Ok(());
        }
        let accelerated = self.accelerated;
        let mut instance = linker.instance(CNN)?;
        instance.func_wrap("backend", move |_, (): ()| {
            Ok((if accelerated {
                Ok("cpu".to_owned())
            } else {
                Err("no accelerator here".to_owned())
            },))
        })?;
        instance.func_wrap_concurrent(
            "run",
            |_, (layers, input): (Vec<cnn::Layer>, cnn::Tensor)| {
                Box::pin(async move { Ok((cnn::run(&layers, input),)) })
            },
        )
    }

    fn island_data(&self, _members: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        Ok(state)
    }
}

#[test]
fn the_reference_network_finds_edges() {
    // A 4x4 image, dark on its left half, bright on its right.
    let data = (0..16).map(|i| if i % 4 < 2 { 0.0 } else { 1.0 }).collect();
    let input = cnn::Tensor {
        channels: 1,
        height: 4,
        width: 4,
        data,
    };
    let sobel = [-1.0, 0.0, 1.0, -2.0, 0.0, 2.0, -1.0, 0.0, 1.0];
    let layers = [
        cnn::Layer::Conv(cnn::Conv {
            out_channels: 1,
            size: 3,
            weights: sobel.to_vec(),
            bias: vec![0.0],
        }),
        cnn::Layer::Relu,
        cnn::Layer::Clamp((0.0, 1.0)),
    ];
    let out = cnn::run(&layers, input).unwrap();
    // Row 1 (away from the border): the edge, between columns 1 and 2.
    assert_eq!(&out.data[4..8], &[0.0, 1.0, 1.0, 0.0]);
}

/// source → edges → sink, on `accel`.
async fn edges(accel: Accel) -> RuntimeGraph<Perf, Accel> {
    let kit = Kit::new(&[VIDEO_SOURCE, EDGE_DETECT, VIDEO_SINK]);
    let graph = kit
        .builder("edges")
        .add_node("source", kit.get(VIDEO_SOURCE))
        .add_node("edges", kit.get(EDGE_DETECT))
        .add_node("sink", kit.get(VIDEO_SINK))
        .connect("a", port("source", "frames"), port("edges", "frames"))
        .connect("b", port("edges", "edges"), port("sink", "frames"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let mut rt =
        RuntimeGraph::load_with_host(compiled, &kit.wasm, RuntimeConfig::default(), Perf, accel)
            .await
            .expect("loads");
    inject(&mut rt, "source", "frames", Val::U32(3)).await;
    rt
}

#[tokio::test]
async fn edges_are_detected_on_the_host_s_accelerator() {
    let mut rt = edges(Accel { accelerated: true }).await;
    settle(&mut rt).await;
    assert_eq!(read_string(&mut rt, "edges", "backend").await, "cpu");
    assert_eq!(read_u32(&mut rt, "sink", "count").await, 3);
    assert!(read_bool(&mut rt, "sink", "gray").await, "edges are gray");
    // The frame's corner is an edge: beyond it, the padding is zeros.
    assert_eq!(read_u32(&mut rt, "sink", "first-pixel").await, 0xffff_ffff);
}

#[tokio::test]
async fn without_an_accelerator_edge_detection_faults_saying_why() {
    let mut rt = edges(Accel { accelerated: false }).await;
    match tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .expect("tick")
    {
        TickResult::Aborted { node, fault } => {
            assert_eq!(node, id("edges"));
            assert!(
                matches!(&fault, NodeFault::Fatal { message } if message.contains("no accelerator here")),
                "{fault:?}"
            );
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[test]
fn a_component_declares_its_viewers_in_a_custom_section() {
    for (guest, viewer) in [
        (VIDEO_SOURCE, Some("image")),
        (FRAME_GRAYSCALE, Some("image")),
        (FRAME_INVERT, Some("image")),
        (RELAY, Some("series")),
        (STREAM_PRODUCER, Some("series")),
        (FUTURE_WRITER, Some("text")),
        (MQTT_NODE, Some("text")),
        (CONFIGURABLE, Some("series")),
    ] {
        let declared = contract(guest);
        let lowered = witgraph_wit::lower_component(guest.wasm, &declared.id)
            .expect("lowers")
            .contract;
        assert_eq!(
            lowered.outputs[0].viewer.as_deref(),
            viewer,
            "{}",
            declared.id
        );
        // WIT text declares no viewer.
        assert_eq!(declared.outputs[0].viewer, None);
    }
    let sink = contract(VIDEO_SINK);
    let lowered = witgraph_wit::lower_component(VIDEO_SINK.wasm, &sink.id).unwrap();
    assert!(lowered.contract.outputs.iter().all(|p| p.viewer.is_none()));
    assert_eq!(lowered.warnings, Vec::<String>::new());
}

#[test]
fn a_wrong_viewer_hint_is_skipped_not_fatal() {
    // A second section at the root, after the module's `output.frames=image`:
    // a stale output, and `frames` again.
    let hints = b"output.frame=image\noutput.frames=text\n";
    let name = witgraph_wit::viewer::SECTION.as_bytes();
    let mut body = vec![u8::try_from(name.len()).unwrap()];
    body.extend_from_slice(name);
    body.extend_from_slice(hints);
    let mut bytes = VIDEO_SOURCE.wasm.to_vec();
    bytes.push(0);
    bytes.push(u8::try_from(body.len()).unwrap());
    bytes.extend(body);

    let id = contract(VIDEO_SOURCE).id;
    let lowered = witgraph_wit::lower_component(&bytes, &id).expect("still lowers");
    assert_eq!(lowered.contract.outputs[0].viewer.as_deref(), Some("image"));
    assert_eq!(lowered.warnings.len(), 2, "{:?}", lowered.warnings);
    assert!(lowered.warnings.iter().any(|w| w.contains("`frame`")));
    assert!(lowered.warnings.iter().any(|w| w.contains("named twice")));
    // The bytes still verify against their contract.
    witgraph_wit::verify::verify_component(&contract(VIDEO_SOURCE), &bytes).expect("verifies");
}

/// The interface `labelled` imports twice.
const KEYVALUE: &str = "wasi:keyvalue/store@0.2.0-draft2";

/// Serves `wasi:keyvalue/store`. A bucket's `get` answers
/// `<plugin id>:<key>#<n>`, where `n` counts the calls made in the island so
/// far, by any store: the count lives in the island's plugin state.
struct KvStore(&'static str);

#[derive(Default)]
struct Calls(u32);

/// The `bucket` resource; `open` answers one, whatever its name.
struct Bucket;

/// `wasi:keyvalue/store`'s `error`.
#[derive(ComponentType, Lift, Lower)]
#[component(variant)]
enum KvError {
    #[component(name = "no-such-store")]
    NoSuchStore,
    #[component(name = "access-denied")]
    AccessDenied,
    #[component(name = "other")]
    Other(String),
}

impl CapabilityPlugin for KvStore {
    fn id(&self) -> &str {
        self.0
    }

    fn provides(&self) -> Vec<String> {
        vec![KEYVALUE.into()]
    }

    fn link(
        &self,
        _node: &NodeId,
        capability: &Capability,
        linker: &mut wasmtime::component::Linker<PluginData>,
    ) -> wasmtime::Result<()> {
        let id = self.0;
        let mut instance = linker.instance(&capability.interface)?;
        instance.resource("bucket", ResourceType::host::<Bucket>(), |_, _| Ok(()))?;
        instance.func_wrap("open", |_, (_identifier,): (String,)| {
            Ok((Ok::<_, KvError>(Resource::<Bucket>::new_own(0)),))
        })?;
        instance.func_wrap(
            "[method]bucket.get",
            move |mut store: StoreContextMut<'_, PluginData>,
                  (_bucket, key): (Resource<Bucket>, String)| {
                let extensions = store.data_mut().extensions();
                let calls = extensions.get::<Calls>().map_or(0, |c| c.0) + 1;
                extensions.insert(Calls(calls));
                let value = format!("{id}:{key}#{calls}").into_bytes();
                Ok((Ok::<_, KvError>(Some(value)),))
            },
        )?;
        Ok(())
    }
}

/// A one-node graph over the `labelled` guest, loaded with `plugins`.
async fn load_labelled(plugins: Plugins) -> Result<RuntimeGraph<Perf, Plugins>, LoadError> {
    let kit = Kit::new(&[LABELLED]);
    let graph = kit
        .builder("labelled")
        .add_node("n", kit.get(LABELLED))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    RuntimeGraph::load_with_host(compiled, &kit.wasm, RuntimeConfig::default(), Perf, plugins).await
}

async fn read_string<M: RuntimeMode, H: Host>(
    rt: &mut RuntimeGraph<M, H>,
    node: &str,
    port: &str,
) -> String {
    match read(rt, node, port).await {
        Some(Val::String(s)) => s,
        other => panic!("expected a string, got {other:?}"),
    }
}

#[tokio::test]
async fn labels_route_one_interface_to_two_plugins() {
    let plugins = Plugins::new()
        .with(KvStore("backup"))
        .unwrap()
        .with(KvStore("primary"))
        .unwrap();
    let mut rt = load_labelled(plugins)
        .await
        .expect("both labels name a plugin");
    inject(&mut rt, "n", "key", Val::String("k".into())).await;
    settle(&mut rt).await;
    // Each label reached the plugin of its name; the call count is the
    // island's, shared by both plugins.
    assert_eq!(read_string(&mut rt, "n", "primary").await, "primary:k#1");
    assert_eq!(read_string(&mut rt, "n", "backup").await, "backup:k#2");
}

#[tokio::test]
async fn labels_naming_no_plugin_reach_the_only_one_serving_the_interface() {
    let plugins = Plugins::new().with(KvStore("only")).unwrap();
    let mut rt = load_labelled(plugins)
        .await
        .expect("one plugin serves both");
    settle(&mut rt).await;
    assert_eq!(read_string(&mut rt, "n", "primary").await, "only:#1");
    assert_eq!(read_string(&mut rt, "n", "backup").await, "only:#2");
}

#[tokio::test]
async fn a_capability_no_plugin_provides_fails_the_load_by_name() {
    let err = load_labelled(Plugins::new())
        .await
        .err()
        .expect("nothing serves the store");
    match &err {
        LoadError::MissingCapability {
            node,
            capability,
            implements,
        } => {
            assert_eq!(node.as_str(), "n");
            assert_eq!(capability, "backup");
            assert_eq!(implements.as_deref(), Some(KEYVALUE));
        }
        other => panic!("expected MissingCapability, got {other}"),
    }
    assert_eq!(
        err.to_string(),
        "node `n` imports capability `backup` (`wasi:keyvalue/store@0.2.0-draft2`), which the host does not provide"
    );
}

#[tokio::test]
async fn a_label_between_two_plugins_it_does_not_name_is_ambiguous() {
    let plugins = Plugins::new()
        .with(KvStore("east"))
        .unwrap()
        .with(KvStore("west"))
        .unwrap();
    let err = load_labelled(plugins)
        .await
        .err()
        .expect("neither label names a plugin");
    match err {
        LoadError::AmbiguousCapability { providers, .. } => {
            assert_eq!(providers, ["east", "west"]);
        }
        other => panic!("expected AmbiguousCapability, got {other}"),
    }
}

/// The `math` provider, as links name it.
fn math() -> ComponentRef {
    "test:math/math@0.1.0".parse().unwrap()
}

/// The interface `calc` imports and `math` exports.
const OPS: &str = "test:math/ops@0.1.0";

/// `calc` and what links may compose into it.
fn calc_kit() -> Kit {
    let mut kit = Kit::new(&[CALC]);
    kit.wasm.insert(math(), MATH.wasm);
    kit
}

#[tokio::test]
async fn a_link_satisfies_an_import_with_a_providers_export() {
    let kit = calc_kit();
    let graph = kit
        .builder("links")
        .add_node("a", kit.get(CALC))
        .add_node("b", kit.get(CALC))
        .link("la", "a", OPS, math(), OPS)
        .link("lb", "b", OPS, math(), OPS)
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    assert!(
        compiled.required_capabilities().is_empty(),
        "the links leave the host nothing to provide"
    );
    // No host: the provider is all `calc` needs.
    let mut rt = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
        .await
        .expect("the provider is composed in");
    inject(&mut rt, "a", "x", Val::U32(5)).await;
    inject(&mut rt, "b", "x", Val::U32(3)).await;
    settle(&mut rt).await;
    // A sync call, an async call and a stream all cross the link.
    assert_eq!(read_u32(&mut rt, "a", "doubled").await, 10);
    assert_eq!(read_u32(&mut rt, "a", "slow").await, 10);
    assert_eq!(read_u32(&mut rt, "a", "total").await, 10);
    assert_eq!(read_u32(&mut rt, "b", "doubled").await, 6);
    assert_eq!(read_u32(&mut rt, "b", "total").await, 3);

    // Each node has a provider of its own, which lives as long as the
    // node's island does.
    assert_eq!(read_u32(&mut rt, "a", "calls").await, 1);
    assert_eq!(read_u32(&mut rt, "b", "calls").await, 1);
    inject(&mut rt, "a", "x", Val::U32(4)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "a", "calls").await, 2);
    assert_eq!(read_u32(&mut rt, "b", "calls").await, 1);
    // A rebuilt island gets a fresh provider with the node.
    rt.cancel(&id("a")).unwrap();
    rt.rerun(&id("a")).unwrap();
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "a", "calls").await, 1);

    // A snapshot names the links: it restores only onto the same ones.
    let snapshot = rt.snapshot().await.unwrap();
    assert_eq!(snapshot.links.len(), 2);
    let mut other = snapshot.clone();
    other.links[0].export = "test:math/other@0.1.0".into();
    let err = rt.restore(&other).await.expect_err("the links differ");
    assert!(
        matches!(err, RuntimeError::SnapshotMismatch { .. }),
        "{err}"
    );
    rt.restore(&snapshot).await.expect("the same links");
}

#[tokio::test]
async fn an_unpinned_provider_is_found_under_its_one_pinned_key() {
    let mut kit = Kit::new(&[CALC]);
    let pinned: ComponentRef = format!("{}#{}", math(), "ab".repeat(32)).parse().unwrap();
    kit.wasm.insert(pinned, MATH.wasm);
    let graph = kit
        .builder("pinned-provider")
        .add_node("a", kit.get(CALC))
        .link("la", "a", OPS, math(), OPS)
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let mut rt = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
        .await
        .expect("the provider is found under its pinned key");
    inject(&mut rt, "a", "x", Val::U32(5)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "a", "doubled").await, 10);
}

/// A provider of `ops` like `math`, whose every function traps.
fn trapping_math() -> &'static [u8] {
    use wit_parser::{LiftLowerAbi, ManglingAndAbi, Resolve};
    let mut resolve = Resolve::default();
    let (package, _) = resolve.push_dir(MATH.wit).unwrap();
    let world = resolve.select_world(&[package], Some("math")).unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, ManglingAndAbi::Legacy(LiftLowerAbi::Sync));
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let bytes = wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap();
    Box::leak(bytes.into_boxed_slice())
}

#[tokio::test]
async fn each_node_runs_the_composition_built_for_its_links() {
    let mut kit = calc_kit();
    let pinned: ComponentRef = format!("{}#{}", math(), "ab".repeat(32)).parse().unwrap();
    kit.wasm.insert(pinned.clone(), trapping_math());
    // `b` comes first and pins the trapping provider; `a` names `math`
    // unpinned, which that pin also matches.
    let graph = kit
        .builder("two-providers")
        .add_node("b", kit.get(CALC))
        .add_node("a", kit.get(CALC))
        .link("lb", "b", OPS, pinned, OPS)
        .link("la", "a", OPS, math(), OPS)
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let mut rt = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
        .await
        .expect("loads");
    inject(&mut rt, "a", "x", Val::U32(5)).await;
    settle(&mut rt).await;
    assert_eq!(
        read_u32(&mut rt, "a", "doubled").await,
        10,
        "`a` runs `math`"
    );
    assert!(
        matches!(
            rt.node_state(&id("b")).unwrap().fault_cause(),
            Some(NodeFault::WasmTrap { .. })
        ),
        "`b` runs the trapping provider"
    );
}

#[tokio::test]
async fn an_unpinned_provider_matching_two_pinned_keys_is_ambiguous() {
    let mut kit = Kit::new(&[CALC]);
    for hash in ["ab", "cd"] {
        let pinned: ComponentRef = format!("{}#{}", math(), hash.repeat(32)).parse().unwrap();
        kit.wasm.insert(pinned, MATH.wasm);
    }
    let graph = kit
        .builder("two-pins")
        .add_node("a", kit.get(CALC))
        .link("la", "a", OPS, math(), OPS)
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let err = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
        .await
        .err()
        .expect("which bytes is not known");
    assert!(
        matches!(&err, LoadError::AmbiguousProvider { candidates, .. } if candidates.len() == 2),
        "{err}"
    );
}

#[tokio::test]
async fn without_a_link_the_import_is_the_hosts() {
    let kit = calc_kit();
    let graph = || kit.builder("unlinked").add_node("n", kit.get(CALC)).build();
    let compiled = graph().compile(&kit.contracts).expect("compiles");
    assert_eq!(compiled.required_capabilities()[0].interface, OPS);
    let err = RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
        .await
        .err()
        .expect("nothing provides `ops`");
    assert!(
        matches!(&err, LoadError::MissingCapability { capability, .. } if capability == OPS),
        "{err}"
    );

    let compiled = graph().compile(&kit.contracts).expect("compiles");
    let err = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Perf,
        Plugins::new(),
    )
    .await
    .err()
    .expect("no plugin provides `ops`");
    assert!(
        matches!(&err, LoadError::MissingCapability { capability, .. } if capability == OPS),
        "{err}"
    );
}

#[tokio::test]
async fn a_link_that_cannot_be_made_fails_the_load() {
    let mut kit = Kit::new(&[CALC, ECHO]);
    kit.wasm.insert(math(), MATH.wasm);
    let load = async |provider: ComponentRef, export: &str| {
        let graph = kit
            .builder("bad-link")
            .add_node("n", kit.get(CALC))
            .link("l", "n", OPS, provider, export)
            .build();
        let compiled = graph.compile(&kit.contracts).expect("compiles");
        RuntimeGraph::load(compiled, &kit.wasm, RuntimeConfig::default(), Perf)
            .await
            .err()
            .expect("the link cannot be made")
    };

    let err = load(math(), "test:math/nope@0.1.0").await;
    assert!(
        matches!(&err, LoadError::BadLink { import, message, .. }
            if import == OPS && message.contains("does not have an export")),
        "{err}"
    );
    // `echo` exports only `node`, which is no `ops`.
    let err = load(kit.get(ECHO), "node").await;
    assert!(
        matches!(&err, LoadError::BadLink { message, .. } if message.contains("cannot take `node`")),
        "{err}"
    );
    let absent: ComponentRef = "test:absent/absent@0.1.0".parse().unwrap();
    let err = load(absent.clone(), OPS).await;
    assert!(
        matches!(&err, LoadError::MissingWasm { component } if **component == absent),
        "{err}"
    );
}

#[tokio::test]
async fn a_linked_node_is_prepared_with_its_links() {
    let kit = calc_kit();
    let contract = &kit.contracts[0];
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let graph = || {
        kit.builder("prepared")
            .add_node("n", kit.get(CALC))
            .link("l", "n", OPS, math(), OPS)
            .build()
            .compile(&kit.contracts)
            .expect("compiles")
    };
    let load = async |prepared: PreparedComponent| {
        RuntimeGraph::load_prepared(
            graph(),
            &[prepared],
            RuntimeConfig::default(),
            Perf,
            witgraph_runtime::NoCapabilities,
        )
        .await
    };

    let bare = PreparedComponent::new(&engine, contract, CALC.wasm).unwrap();
    let err = load(bare).await.err().expect("prepared without the link");
    assert!(
        matches!(&err, LoadError::ContractMismatch { message, .. }
            if message.contains("not prepared with the links of node `n`")),
        "{err}"
    );

    let provider = math();
    let links = [LinkedProvider {
        import: OPS,
        provider: &provider,
        bytes: MATH.wasm,
        export: OPS,
    }];
    let linked = PreparedComponent::linked(&engine, contract, CALC.wasm, &links).unwrap();
    let mut rt = load(linked).await.expect("prepared with the link");
    inject(&mut rt, "n", "x", Val::U32(2)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "n", "doubled").await, 4);
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
        BYTES_CONSUMER,
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
        // mqtt → bytes-consumer
        .add_node("mqtt", kit.get(MQTT_NODE))
        .add_node("count", kit.get(BYTES_CONSUMER))
        .connect("m1", port("mqtt", "messages"), port("count", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    // cfg, echo_a, echo_b, {prod, cons}, plus, sink_a, sink_b, {mqtt, count}
    assert_eq!(compiled.islands().len(), 8);
    let feed = Arc::new(Mutex::new(
        (0..4u32).map(|i| vec![i as u8]).collect::<VecDeque<_>>(),
    ));
    let mut rt = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Trace::new(),
        mqtt(feed),
    )
    .await
    .expect("loads");
    inject(&mut rt, "cfg", "rate", Val::U32(4)).await;
    inject(&mut rt, "cfg", "gain", Val::Float64(3.0)).await;
    inject(&mut rt, "prod", "burst-size", Val::U32(8)).await;
    inject(&mut rt, "plus", "add", Val::U32(1)).await;
    let ticks = settle(&mut rt).await;
    assert!(
        ticks <= 3,
        "no feedback, so one productive tick: took {ticks}"
    );

    assert_eq!(read_f64(&mut rt, "echo_b", "out").await, 12.0, "4 * 3.0");
    assert_eq!(read_u32(&mut rt, "cons", "total").await, 28, "sum(0..8)");
    assert_eq!(read_u32(&mut rt, "sink_a", "out").await, 29);
    assert_eq!(read_u32(&mut rt, "sink_b", "out").await, 29);
    assert_eq!(
        read_u32(&mut rt, "count", "count").await,
        4,
        "4 byte messages from mqtt feed"
    );
    let trace = rt.mode().trace();
    assert!(faults(&trace).is_empty(), "{:?}", faults(&trace));
    assert_eq!(
        runs_of(&trace, "sink_a"),
        1,
        "downstream waits for upstream"
    );
}

// ---- Scheduling across islands ----

#[tokio::test]
async fn a_value_latched_mid_generation_wakes_downstream_islands() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER, RELAY]);
    let graph = kit
        .builder("endless")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .add_node("r", kit.get(RELAY))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("l", port("prod", "limit"), port("r", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    // Endless: no burst-size, no take.
    let tick = tokio::time::timeout(Duration::from_secs(2), rt.tick()).await;
    assert!(
        tick.is_err(),
        "the endless generation keeps the tick running"
    );
    assert_eq!(phase(&rt, "cons"), NodePhase::Running);
    assert_eq!(
        read_u32(&mut rt, "r", "out").await,
        u32::MAX,
        "`r` ran on `limit` while the stream still flows"
    );
    assert_eq!(phase(&rt, "r"), NodePhase::Idle);
}

#[tokio::test]
async fn an_endless_island_owing_another_run_does_not_hold_back_downstream() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER, RELAY]);
    let graph = kit
        .builder("endless")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .add_node("r", kit.get(RELAY))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("l", port("prod", "limit"), port("r", "in"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), rt.tick()).await;
    assert_eq!(read_u32(&mut rt, "r", "out").await, u32::MAX);

    // The endless island now owes another run it can never start.
    inject(&mut rt, "cons", "delay", Val::U32(1)).await;
    inject(&mut rt, "r", "add", Val::U32(1)).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), rt.tick()).await;
    assert_eq!(
        read_u32(&mut rt, "r", "out").await,
        0,
        "u32::MAX + 1, wrapping"
    );
}

#[tokio::test]
async fn a_value_round_trip_through_another_node_runs_in_one_generation() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER, RELAY]);
    let graph = kit
        .builder("round-trip")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .add_node("r", kit.get(RELAY))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("l", port("prod", "limit"), port("r", "in"))
        .connect("t", port("r", "out"), port("cons", "take"))
        .build();
    let compiled = graph.clone().compile(&kit.contracts).expect("compiles");
    assert_eq!(compiled.islands().len(), 1, "`r` joins the stream island");
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "prod", "burst-size", Val::U32(10)).await;
    // take = 10 + add, wrapping: 3.
    inject(&mut rt, "r", "add", Val::U32(u32::MAX - 6)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "count").await, 3);
    assert_eq!(read_u32(&mut rt, "cons", "total").await, 3, "0+1+2");
    assert_eq!(
        runs_of(&rt.mode().trace(), "cons"),
        1,
        "`cons` ran once, on `take` fresh from this generation"
    );
}

#[tokio::test]
async fn a_node_reading_an_island_that_will_rerun_waits_for_it() {
    let kit = Kit::new(&[BUSY_LOOP, RELAY, STREAM_PRODUCER, STREAM_CONSUMER, MAYBE]);
    let graph = kit
        .builder("whole-island")
        .add_node("x", kit.get(BUSY_LOOP))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("r", kit.get(RELAY))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .add_node("j", kit.get(MAYBE))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .connect("l", port("prod", "limit"), port("r", "add"))
        .connect("t", port("r", "out"), port("cons", "take"))
        .connect("d", port("x", "done"), port("r", "in"))
        .connect("jx", port("prod", "limit"), port("j", "x"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "prod", "burst-size", Val::U32(3)).await;
    settle(&mut rt).await;
    assert_eq!(
        runs_of(&rt.mode().trace(), "j"),
        1,
        "`prod` reruns with its island once `x` delivers, so `j` waits for it"
    );
}

#[tokio::test]
async fn graphs_can_share_an_engine() {
    let kit = Kit::new(&[ECHO]);
    let graph = || kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let first = kit.load(graph(), RuntimeConfig::default(), Perf).await;
    let config = RuntimeConfig {
        engine: Some(first.engine().clone()),
        ..RuntimeConfig::default()
    };
    let mut second = kit.load(graph(), config, Perf).await;
    assert!(wasmtime::Engine::same(first.engine(), second.engine()));
    inject(&mut second, "e", "in", Val::Float64(2.0)).await;
    settle(&mut second).await;
    assert_eq!(read_f64(&mut second, "e", "out").await, 2.0);
}

#[tokio::test]
async fn an_engine_that_cannot_run_a_graph_is_rejected() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let config = RuntimeConfig {
        engine: Some(wasmtime::Engine::default()),
        ..RuntimeConfig::default()
    };
    let err = RuntimeGraph::load(compiled, &kit.wasm, config, Perf)
        .await
        .err()
        .expect("the default engine meters no fuel");
    assert!(
        matches!(err, LoadError::Runtime(RuntimeError::InvalidConfig { ref message }) if message.contains("fuel")),
        "{err}"
    );
}

#[tokio::test]
async fn bytes_can_be_keyed_by_an_unhashed_ref() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let mut unhashed = kit.get(ECHO);
    unhashed.content_hash = None;
    let wasm = HashMap::from([(unhashed, ECHO.wasm)]);
    let mut rt = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Trace::new())
        .await
        .expect("the unhashed key matches by package and world");
    inject(&mut rt, "e", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    let phases: Vec<(NodePhase, NodePhase)> = rt
        .mode()
        .trace()
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PhaseTransition { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert_eq!(
        phases,
        [
            (NodePhase::Pending, NodePhase::Running),
            (NodePhase::Running, NodePhase::Idle)
        ]
    );
}

#[tokio::test]
async fn a_cycle_through_three_stream_islands_runs() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut builder = kit.builder("three");
    for i in 1..=3 {
        builder = builder
            .add_node(format!("s{i}").as_str(), kit.get(STREAM_PRODUCER))
            .add_node(format!("t{i}").as_str(), kit.get(STREAM_CONSUMER))
            .connect(
                format!("st{i}").as_str(),
                port(&format!("s{i}"), "items"),
                port(&format!("t{i}"), "items"),
            );
    }
    let graph = builder
        .connect("v12", port("s1", "limit"), port("t2", "take"))
        .connect("v23", port("s2", "limit"), port("t3", "take"))
        .connect("v31", port("s3", "limit"), port("t1", "take"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    assert_eq!(rt.compiled().islands().len(), 1);
    inject(&mut rt, "s1", "burst-size", Val::U32(2)).await;
    inject(&mut rt, "s2", "burst-size", Val::U32(3)).await;
    inject(&mut rt, "s3", "burst-size", Val::U32(4)).await;
    settle(&mut rt).await;
    assert_eq!(
        read_u32(&mut rt, "t1", "count").await,
        2,
        "s1 streams only 2"
    );
    assert_eq!(
        read_u32(&mut rt, "t2", "count").await,
        2,
        "takes s1's limit"
    );
    assert_eq!(
        read_u32(&mut rt, "t3", "count").await,
        3,
        "takes s2's limit"
    );
    let trace = rt.mode().trace();
    for node in ["t1", "t2", "t3"] {
        assert_eq!(runs_of(&trace, node), 1, "{node}");
    }
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    // `a.in` is required: fed by the feedback edge, seeded by the host.
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    for i in 1..=5u32 {
        assert!(matches!(rt.tick().await, TickResult::Progress));
        assert_eq!(read_u32(&mut rt, "b", "out").await, i, "iteration {i}");
    }
    let trace = rt.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 5);
    assert_eq!(runs_of(&trace, "b"), 5);
}

/// A relay loop (`a -> b`, feedback `b -> a`) beside an endless pipeline,
/// so a tick never reaches its end-of-tick latch.
fn loop_beside_endless_pipeline(kit: &Kit) -> Graph {
    let relay = kit.get(RELAY);
    kit.builder("loop+endless")
        .add_node("a", relay.clone())
        .add_node("b", relay)
        .connect("ab", port("a", "out"), port("b", "in"))
        .connect_feedback("ba", port("b", "out"), port("a", "in"))
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .add_node("cons", kit.get(STREAM_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build()
}

#[tokio::test]
async fn shutdown_forgets_feedback_buffered_by_an_unfinished_tick() {
    let kit = Kit::new(&[RELAY, STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(
            loop_beside_endless_pipeline(&kit),
            RuntimeConfig::default(),
            Trace::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    let tick = tokio::time::timeout(Duration::from_millis(500), rt.tick()).await;
    assert!(tick.is_err(), "the endless pipeline keeps the tick running");
    assert_eq!(read_u32(&mut rt, "b", "out").await, 1);
    let snapshot = rt.snapshot().await.unwrap();
    assert_eq!(
        snapshot.inputs[&id("a")][&"in".into()],
        "1",
        "the loop's iteration was over, so the dropped tick latched it"
    );
    assert_eq!(snapshot.islands.len(), 2, "`a` owes its next iteration");

    rt.shutdown();
    assert!(rt.snapshot().await.unwrap().feedback.is_empty());
    assert!(matches!(rt.tick().await, TickResult::Idle));
    assert_eq!(read_u32(&mut rt, "b", "out").await, 1);
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 1, "nothing restarted");
}

#[tokio::test]
async fn feedback_loops_advance_beside_an_island_that_never_finishes() {
    let kit = Kit::new(&[RELAY, STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(
            loop_beside_endless_pipeline(&kit),
            RuntimeConfig::default(),
            Trace::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    for i in 1..=3u32 {
        let tick = tokio::time::timeout(Duration::from_millis(300), rt.tick()).await;
        assert!(tick.is_err(), "the pipeline never finishes");
        assert_eq!(
            read_u32(&mut rt, "b", "out").await,
            i,
            "one iteration per tick"
        );
    }
}

#[tokio::test]
async fn a_value_injected_between_ticks_is_not_overwritten_by_older_feedback() {
    let kit = Kit::new(&[RELAY, STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(
            loop_beside_endless_pipeline(&kit),
            RuntimeConfig::default(),
            Trace::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    let _ = tokio::time::timeout(Duration::from_millis(300), rt.tick()).await;
    assert_eq!(read_u32(&mut rt, "b", "out").await, 1);
    // `a.in` is fed by feedback, so the host may seed it again.
    inject(&mut rt, "a", "in", Val::U32(100)).await;
    let _ = tokio::time::timeout(Duration::from_millis(300), rt.tick()).await;
    assert_eq!(read_u32(&mut rt, "b", "out").await, 101);
}

#[tokio::test]
async fn step_limited_ticks_finish_an_iteration_before_the_next() {
    let kit = Kit::new(&[RELAY]);
    let relay = kit.get(RELAY);
    let graph = kit
        .builder("self-loop")
        .add_node("a", relay.clone())
        .add_node("c", relay)
        .connect_feedback("aa", port("a", "out"), port("a", "in"))
        .connect("ac", port("a", "out"), port("c", "in"))
        .build();
    let config = RuntimeConfig {
        max_steps_per_tick: 1,
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Trace::new()).await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    for _ in 0..10 {
        let _ = rt.tick().await;
    }
    let a = read_u32(&mut rt, "a", "out").await;
    assert!(
        read_u32(&mut rt, "c", "out").await + 1 >= a,
        "`c` keeps up with `a`: a={a}"
    );
}

/// `a` loops on itself (`a.out -> a.in`, feedback) and feeds `c`; with one
/// step per tick, a tick runs `a`, buffers its feedback, and stops before
/// `c` runs.
async fn step_limited_self_loop(kit: &Kit) -> RuntimeGraph<Trace> {
    let relay = kit.get(RELAY);
    let graph = kit
        .builder("self-loop")
        .add_node("a", relay.clone())
        .add_node("c", relay)
        .connect_feedback("aa", port("a", "out"), port("a", "in"))
        .connect("ac", port("a", "out"), port("c", "in"))
        .build();
    let config = RuntimeConfig {
        max_steps_per_tick: 1,
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Trace::new()).await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    assert!(matches!(rt.tick().await, TickResult::StepLimitReached));
    assert_eq!(rt.snapshot().await.unwrap().feedback.len(), 1, "buffered");
    rt
}

#[tokio::test]
async fn cancel_forgets_buffered_feedback() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = step_limited_self_loop(&kit).await;
    rt.cancel(&id("a")).unwrap();
    assert!(rt.snapshot().await.unwrap().feedback.is_empty());
    for _ in 0..5 {
        let _ = rt.tick().await;
    }
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 1, "`a` stays cancelled");
}

#[tokio::test]
async fn shutdown_forgets_buffered_feedback() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = step_limited_self_loop(&kit).await;
    rt.shutdown();
    assert!(rt.snapshot().await.unwrap().feedback.is_empty());
    for _ in 0..5 {
        let _ = rt.tick().await;
    }
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 1);
}

#[tokio::test]
async fn an_injected_value_outlives_feedback_buffered_before_it() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = step_limited_self_loop(&kit).await;
    // The buffered `a.out = 1` is older than this write.
    inject(&mut rt, "a", "in", Val::U32(100)).await;
    let _ = rt.tick().await;
    assert_eq!(read_u32(&mut rt, "a", "out").await, 101);
}

#[tokio::test]
async fn cancel_forgets_feedback_buffered_for_the_island() {
    let kit = Kit::new(&[RELAY, STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(
            loop_beside_endless_pipeline(&kit),
            RuntimeConfig::default(),
            Trace::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    let _ = tokio::time::timeout(Duration::from_millis(500), rt.tick()).await;
    rt.cancel(&id("a")).unwrap();
    rt.cancel(&id("cons")).unwrap();
    assert!(matches!(rt.tick().await, TickResult::Idle));
    assert_eq!(phase(&rt, "a"), NodePhase::Cancelled);
    assert_eq!(runs_of(&rt.mode().trace(), "a"), 1);
}

// ---- Faults, fuel, fatal ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spinning_guest_yields_so_a_timeout_fires() {
    let kit = Kit::new(&[BUSY_LOOP]);
    let graph = kit
        .builder("spin")
        .add_node("busy", kit.get(BUSY_LOOP))
        .build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into())).await;
    // The tick runs on a task of its own: if the guest never yielded, that
    // task would never return, and the watchdog below fails the test
    // instead of hanging it.
    let task = tokio::spawn(async move {
        let tick = tokio::time::timeout(Duration::from_millis(300), rt.tick()).await;
        (tick.is_err(), rt)
    });
    let (timed_out, mut rt) = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the timeout around the tick fired")
        .expect("the tick task did not panic");
    assert!(timed_out);
    rt.cancel(&id("busy")).unwrap();
}

#[tokio::test]
async fn an_island_over_its_memory_limit_fails_to_instantiate() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("small").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let config = RuntimeConfig {
        max_island_memory: Some(4096),
        ..RuntimeConfig::default()
    };
    let err = RuntimeGraph::load(compiled, &kit.wasm, config, Perf)
        .await
        .err()
        .expect("echo's memory is bigger than 4 KiB");
    assert!(
        matches!(err, LoadError::Instantiation { ref message, .. } if message.contains("limit")),
        "{err}"
    );
}

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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into())).await;
    inject(&mut rt, "e", "in", Val::Float64(1.5)).await;
    inject(&mut rt, "prod", "burst-size", Val::U32(3)).await;
    settle(&mut rt).await;

    let busy = rt.node_state(&id("busy")).unwrap();
    assert_eq!(busy.phase(), NodePhase::Faulted);
    assert!(matches!(
        busy.fault_cause(),
        Some(NodeFault::WasmTrap { .. })
    ));
    assert_eq!(read_f64(&mut rt, "e", "out").await, 1.5);
    assert_eq!(read_u32(&mut rt, "cons", "total").await, 3);
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
        attrs: Default::default(),
    });
    graph
}

#[tokio::test]
async fn busy_loop_exhausts_its_run_budget_while_a_sibling_island_finishes() {
    let kit = Kit::new(&[BUSY_LOOP, STREAM_PRODUCER, STREAM_CONSUMER]);
    let config = RuntimeConfig {
        yield_interval: 10_000,
        fuel_per_run: Some(5_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit
        .load(pipeline_and_busy(&kit), config, Trace::new())
        .await;
    feed_pipeline(&mut rt, Some(100), None).await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into())).await;
    settle(&mut rt).await;

    let busy = rt.node_state(&id("busy")).unwrap();
    assert!(
        matches!(busy.fault_cause(), Some(NodeFault::FuelExhausted)),
        "{:?}",
        busy.fault_cause()
    );
    assert_eq!(read_u32(&mut rt, "cons", "count").await, 100);
    assert_eq!(
        read_u32(&mut rt, "cons", "total").await,
        (0..100).sum::<u32>()
    );
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
    let mut rt = kit.load(pipeline(&kit), config, Trace::new()).await;
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("fatal".into())).await;
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into())).await;
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Faulted);

    rt.inject(&id("busy"), &"action".into(), Val::Enum("finish".into()))
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Idle);
    assert_eq!(read_u32(&mut rt, "busy", "done").await, 1);
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
        yield_interval: 10_000,
        fuel_per_run: Some(200_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Trace::new()).await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into())).await;
    poll_tick_once(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Running);

    // Owed while the spinning generation runs; the fault must not drop it.
    inject(&mut rt, "busy", "action", Val::Enum("finish".into())).await;
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Idle);
    assert_eq!(read_u32(&mut rt, "busy", "done").await, 1);
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
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into())).await;
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
    assert_eq!(cancels, 1, "cancelling a cancelled island reports nothing");
}

#[tokio::test]
async fn cancelling_again_forgets_work_owed_since() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("recancel").add_node("e", kit.get(ECHO)).build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "e", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    rt.cancel(&id("e")).unwrap();
    inject(&mut rt, "e", "in", Val::Float64(2.0)).await;
    rt.cancel(&id("e")).unwrap();
    assert!(matches!(rt.tick().await, TickResult::Idle));
    assert_eq!(phase(&rt, "e"), NodePhase::Cancelled);
    assert_eq!(read_f64(&mut rt, "e", "out").await, 1.0);
}

// ---- Cancellation and shutdown ----

#[tokio::test]
async fn cancel_drops_an_endless_island_and_new_input_restarts_it() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    // Endless: no burst-size, no take.
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Trace::new())
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

    rt.inject(&id("prod"), &"burst-size".into(), Val::U32(4))
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "total").await, 6, "0+1+2+3");
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
        attrs: Default::default(),
    });
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    let _ = tokio::time::timeout(Duration::from_millis(200), rt.tick()).await;
    assert_eq!(phase(&rt, "e"), NodePhase::Idle);

    rt.shutdown();
    for node in ["prod", "cons", "e"] {
        assert_eq!(phase(&rt, node), NodePhase::Cancelled, "{node}");
    }
    assert!(matches!(rt.tick().await, TickResult::Idle));
}

#[tokio::test]
async fn rerun_restarts_an_island_the_host_cannot_feed() {
    let kit = Kit::new(&[MQTT_NODE, BYTES_CONSUMER]);
    let graph = kit
        .builder("mqtt")
        .add_node("mqtt", kit.get(MQTT_NODE))
        .add_node("cons", kit.get(BYTES_CONSUMER))
        .connect("s", port("mqtt", "messages"), port("cons", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let feed = Arc::new(Mutex::new(VecDeque::from([vec![1u8], vec![2u8]])));
    let mut rt = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Perf,
        mqtt(feed.clone()),
    )
    .await
    .expect("loads");
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "total-bytes").await, 2);
    rt.cancel(&id("mqtt")).unwrap();

    feed.lock()
        .unwrap()
        .extend([vec![10u8, 20], vec![30u8, 40]]);
    assert!(
        matches!(rt.tick().await, TickResult::Idle),
        "nothing is owed"
    );
    rt.rerun(&id("mqtt")).unwrap();
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "total-bytes").await, 4);
}

#[tokio::test]
async fn clear_input_forgets_an_injected_value() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("clear").add_node("e", kit.get(ECHO)).build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "e", "in", Val::Float64(2.0)).await;
    settle(&mut rt).await;
    rt.clear_input(&id("e"), &"in".into()).await.unwrap();
    settle(&mut rt).await;
    assert_eq!(
        read_f64(&mut rt, "e", "out").await,
        0.0,
        "`in` reads as none again"
    );
    rt.clear_input(&id("e"), &"in".into()).await.unwrap();
    assert!(
        matches!(rt.tick().await, TickResult::Idle),
        "clearing nothing is not a change"
    );
}

/// Records the members of every island Store it creates data for.
struct Counting {
    built: Arc<Mutex<Vec<Vec<NodeId>>>>,
}

struct Counted {
    state: HostState,
}

impl witgraph_runtime::IslandData for Counted {
    fn host_state(&mut self) -> &mut HostState {
        &mut self.state
    }
}

impl Host for Counting {
    type Data = Counted;

    fn link(
        &self,
        _node: &NodeId,
        _contract: &ComponentContract,
        _linker: &mut wasmtime::component::Linker<Counted>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }

    fn island_data(&self, members: &[NodeId], state: HostState) -> wasmtime::Result<Counted> {
        self.built.lock().unwrap().push(members.to_vec());
        Ok(Counted { state })
    }
}

#[tokio::test]
async fn the_host_creates_the_data_of_every_island_store() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit
        .builder("host")
        .add_node("a", kit.get(ECHO))
        .add_node("b", kit.get(ECHO))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let built = Arc::new(Mutex::new(Vec::new()));
    let mut rt = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        RuntimeConfig::default(),
        Perf,
        Counting {
            built: built.clone(),
        },
    )
    .await
    .expect("loads");
    assert_eq!(built.lock().unwrap().len(), 2, "one Store per island");
    settle(&mut rt).await;
    rt.cancel(&id("a")).unwrap();
    inject(&mut rt, "a", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "a", "out").await, 1.0);
    assert_eq!(
        built.lock().unwrap().last(),
        Some(&vec![id("a")]),
        "a rebuild gets fresh data"
    );
}

#[tokio::test]
async fn cancel_frees_what_the_island_held_back() {
    let kit = Kit::new(&[BUSY_LOOP, MAYBE]);
    let graph = kit
        .builder("held")
        .add_node("busy", kit.get(BUSY_LOOP))
        .add_node("m", kit.get(MAYBE))
        .connect("d", port("busy", "done"), port("m", "x"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into())).await;
    let tick = rt
        .tick_until(tokio::time::sleep(Duration::from_millis(200)))
        .await;
    assert!(matches!(tick, TickResult::Interrupted), "{tick:?}");
    assert_eq!(phase(&rt, "m"), NodePhase::Pending, "held back by `busy`");

    rt.cancel(&id("busy")).unwrap();
    assert!(matches!(rt.tick().await, TickResult::Progress));
    assert_eq!(
        phase(&rt, "m"),
        NodePhase::Idle,
        "nothing holds it back now"
    );
}

#[tokio::test]
async fn cancel_releases_resources_to_waiting_islands() {
    let kit = Kit::new(&[BUSY_LOOP, ECHO]);
    let graph = kit
        .builder("gpu")
        .add_node("busy", kit.get(BUSY_LOOP))
        .add_node("e", kit.get(ECHO))
        .set_resource("busy", "gpu", ResourceClaim::new(1.0).unwrap())
        .unwrap()
        .set_resource("e", "gpu", ResourceClaim::new(1.0).unwrap())
        .unwrap()
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("spin".into())).await;
    inject(&mut rt, "e", "in", Val::Float64(4.0)).await;
    let _ = rt
        .tick_until(tokio::time::sleep(Duration::from_millis(200)))
        .await;
    let e_ran = read(&mut rt, "e", "out").await.is_some();
    if phase(&rt, "busy") == NodePhase::Running {
        assert!(!e_ran, "`e` waits for the gpu");
        rt.cancel(&id("busy")).unwrap();
        let _ = rt.tick().await;
    }
    assert_eq!(read_f64(&mut rt, "e", "out").await, 4.0);
}

#[tokio::test]
async fn a_faulted_island_releases_its_resources() {
    let kit = Kit::new(&[BUSY_LOOP, ECHO]);
    let graph = kit
        .builder("disk")
        .add_node("busy", kit.get(BUSY_LOOP))
        .add_node("e", kit.get(ECHO))
        .set_resource("busy", "disk", ResourceClaim::new(0.6).unwrap())
        .unwrap()
        .set_resource("e", "disk", ResourceClaim::new(0.6).unwrap())
        .unwrap()
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "busy", "action", Val::Enum("trap".into())).await;
    inject(&mut rt, "e", "in", Val::Float64(2.0)).await;
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "busy"), NodePhase::Faulted);
    assert_eq!(
        read_f64(&mut rt, "e", "out").await,
        2.0,
        "the trap freed the disk"
    );
    let trace = rt.mode().trace();
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::GenerationStopped { .. })),
        "a faulted generation is reported stopped"
    );
    let busy = rt.node_state(&id("busy")).unwrap();
    assert_eq!(busy.culprit(), Some(&id("busy")));
    let faults = rt.take_faults();
    assert!(
        matches!(
            faults.as_slice(),
            [report] if report.members == [id("busy")]
                && report.culprit == Some(id("busy"))
                && matches!(report.fault, NodeFault::WasmTrap { .. })
        ),
        "{faults:?}"
    );
    assert!(rt.take_faults().is_empty(), "taken");
}

/// `prod` streaming records to `cons`: a stream the host cannot move, so
/// the two share a Store.
fn reading_pipeline(kit: &Kit) -> Graph {
    kit.builder("readings")
        .add_node("prod", kit.get(READING_PRODUCER))
        .add_node("cons", kit.get(READING_CONSUMER))
        .connect("s", port("prod", "items"), port("cons", "items"))
        .build()
}

#[tokio::test]
async fn a_fault_in_a_member_s_own_store_is_pinned_on_it() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let config = RuntimeConfig {
        fuel_per_run: Some(5_000_000),
        ..split()
    };
    let mut rt = kit.load(pipeline(&kit), config, Perf).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .expect("fuel ends the endless generation");
    // Split, a stream of `u32` is pumped between Stores, so each member has
    // its own: the one whose Store ran out of fuel is known.
    let faults = rt.take_faults();
    let [report] = faults.as_slice() else {
        panic!("one island faulted: {faults:?}");
    };
    assert!(
        matches!(report.fault, NodeFault::FuelExhausted),
        "{report:?}"
    );
    let culprit = report.culprit.clone().expect("the member is known");
    assert!(culprit == id("prod") || culprit == id("cons"), "{culprit}");
    // The island still faults as one.
    for node in ["prod", "cons"] {
        let state = rt.node_state(&id(node)).unwrap();
        assert_eq!(state.phase(), NodePhase::Faulted, "{node}");
        assert_eq!(state.culprit(), Some(&culprit), "{node}");
    }
}

#[tokio::test]
async fn by_default_an_island_is_one_store() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let config = RuntimeConfig {
        fuel_per_run: Some(5_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(pipeline(&kit), config, Perf).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .expect("fuel ends the endless generation");
    // One Store, as if the stream could not be pumped: no member is singled
    // out.
    let faults = rt.take_faults();
    assert!(
        matches!(faults.as_slice(), [report] if report.culprit.is_none()
            && matches!(report.fault, NodeFault::FuelExhausted)),
        "{faults:?}"
    );
}

#[tokio::test]
async fn a_trap_in_a_shared_store_has_no_culprit() {
    let kit = Kit::new(&[READING_PRODUCER, READING_CONSUMER]);
    let config = RuntimeConfig {
        fuel_per_run: Some(5_000_000),
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(reading_pipeline(&kit), config, Perf).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .expect("fuel ends the endless generation");
    // The producer returned and writes from a task; the consumer is still
    // in `run`. In one Store, either may have burned the fuel.
    for node in ["prod", "cons"] {
        assert_eq!(rt.node_state(&id(node)).unwrap().culprit(), None, "{node}");
    }
    let faults = rt.take_faults();
    assert!(
        matches!(faults.as_slice(), [report] if report.culprit.is_none()
            && matches!(report.fault, NodeFault::FuelExhausted)),
        "{faults:?}"
    );
}

#[tokio::test]
async fn split_members_share_a_store_only_for_streams_the_host_cannot_move() {
    let kit = Kit::new(&[
        STREAM_PRODUCER,
        STREAM_CONSUMER,
        READING_PRODUCER,
        READING_CONSUMER,
    ]);
    let graph = kit
        .builder("stores")
        .add_node("numbers", kit.get(STREAM_PRODUCER))
        .add_node("sum", kit.get(STREAM_CONSUMER))
        .add_node("readings", kit.get(READING_PRODUCER))
        .add_node("total", kit.get(READING_CONSUMER))
        .connect("n", port("numbers", "items"), port("sum", "items"))
        .connect("r", port("readings", "items"), port("total", "items"))
        .build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    assert_eq!(compiled.islands().len(), 2, "two stream islands");
    let built = Arc::new(Mutex::new(Vec::new()));
    let mut rt = RuntimeGraph::load_with_host(
        compiled,
        &kit.wasm,
        split(),
        Trace::new().with_stream_items(),
        Counting {
            built: built.clone(),
        },
    )
    .await
    .expect("loads");
    // `u32` items are pumped between two Stores; records stay in one.
    let mut stores = built.lock().unwrap().clone();
    stores.sort();
    assert_eq!(
        stores,
        [
            vec![id("numbers")],
            vec![id("readings"), id("total")],
            vec![id("sum")],
        ]
    );

    inject(&mut rt, "numbers", "burst-size", Val::U32(5)).await;
    inject(&mut rt, "readings", "burst-size", Val::U32(5)).await;
    settle(&mut rt).await;
    for consumer in ["sum", "total"] {
        assert_eq!(read_u32(&mut rt, consumer, "count").await, 5, "{consumer}");
        assert_eq!(read_u32(&mut rt, consumer, "total").await, 10, "{consumer}");
    }
    // The host saw the pumped items pass, and only those.
    let mut passed = 0;
    for event in rt.mode().trace() {
        if let TraceEvent::StreamItems { node, port, count } = event {
            assert_eq!((node, port), (id("numbers"), "items".into()));
            passed += count;
        }
    }
    assert_eq!(passed, 5);
}

#[tokio::test]
async fn node_state_reports_an_unknown_node() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    assert!(matches!(
        rt.node_state(&id("ghost")),
        Err(RuntimeError::UnknownNode { .. })
    ));
}

#[tokio::test]
async fn values_inject_as_wave_text_against_the_port_type() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    assert_eq!(
        rt.input_type(&id("e"), &"in".into()).unwrap(),
        &wasmtime::component::Type::Float64,
        "an optional port's payload type"
    );
    assert!(matches!(
        rt.output_type(&id("e"), &"out".into()).unwrap(),
        wasmtime::component::Type::Float64
    ));
    rt.inject_wave(&id("e"), &"in".into(), "2.5").await.unwrap();
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "e", "out").await, 2.5);
    assert!(matches!(
        rt.inject_wave(&id("e"), &"in".into(), "\"no\"").await,
        Err(RuntimeError::ValueType { .. })
    ));
    assert!(matches!(
        rt.input_type(&id("e"), &"out".into()),
        Err(RuntimeError::NotAValuePort {
            direction: PortDirection::Input,
            ..
        })
    ));
}

/// Records generation starts and ends outside the graph, so they outlive
/// it.
struct Generations(Arc<Mutex<Vec<(usize, u64, bool)>>>);

impl RuntimeMode for Generations {
    fn on_generation_started(&self, island: usize, generation: u64) {
        self.0.lock().unwrap().push((island, generation, true));
    }

    fn on_generation_finished(&self, island: usize, generation: u64) {
        self.0.lock().unwrap().push((island, generation, false));
    }

    fn on_generation_stopped(&self, island: usize, generation: u64) {
        self.0.lock().unwrap().push((island, generation, false));
    }
}

#[tokio::test]
async fn dropping_a_graph_stops_its_generations() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut rt = kit
        .load(
            pipeline(&kit),
            RuntimeConfig::default(),
            Generations(log.clone()),
        )
        .await;
    let tick = rt
        .tick_until(tokio::time::sleep(Duration::from_millis(100)))
        .await;
    assert!(matches!(tick, TickResult::Interrupted), "{tick:?}");
    drop(rt);
    assert_eq!(*log.lock().unwrap(), [(0, 1, true), (0, 1, false)]);
}

#[tokio::test]
async fn read_output_reports_bad_targets() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    assert!(matches!(
        rt.read_output(&id("ghost"), &"out".into()).await,
        Err(RuntimeError::UnknownNode { .. })
    ));
    assert!(matches!(
        rt.read_output(&id("e"), &"in".into()).await,
        Err(RuntimeError::NotAValuePort {
            direction: PortDirection::Output,
            ..
        })
    ));
    assert_eq!(
        rt.read_output(&id("e"), &"out".into()).await.unwrap(),
        None,
        "not run yet"
    );
}

#[tokio::test]
async fn a_tiny_hostcall_budget_faults_a_run_that_returns_a_record() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let config = RuntimeConfig {
        hostcall_fuel: 16,
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Perf).await;
    let _ = rt.tick().await;
    let fault = rt.node_state(&id("e")).unwrap().fault_cause().cloned();
    assert!(
        matches!(fault, Some(NodeFault::HostcallFuelExhausted)),
        "{fault:?}"
    );
}

#[tokio::test]
async fn by_default_a_list_of_megabytes_leaves_a_node() {
    // Lifting a `list<u8>` is charged ~48 bytes of hostcall fuel a byte: a
    // default of 128 MiB stopped one of 4 MiB.
    let kit = Kit::new(&[BLOB_SOURCE, BLOB_SINK]);
    let graph = kit
        .builder("blob")
        .add_node("source", kit.get(BLOB_SOURCE))
        .add_node("sink", kit.get(BLOB_SINK))
        .connect("c", port("source", "blob"), port("sink", "blob"))
        .build();
    let mut rt = kit.load(graph, RuntimeConfig::default(), Perf).await;
    inject(&mut rt, "source", "size", Val::U32(4 << 20)).await;
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "sink", "len").await, 4 << 20);
}

#[tokio::test]
async fn an_unconsumed_endless_stream_is_closed() {
    let kit = Kit::new(&[STREAM_PRODUCER]);
    let graph = kit
        .builder("lone")
        .add_node("prod", kit.get(STREAM_PRODUCER))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    // No burst-size: it would write forever, but nobody reads.
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "prod"), NodePhase::Idle);
}

#[tokio::test]
async fn feedback_unwraps_an_option_into_an_optional_input() {
    let kit = Kit::new(&[MAYBE]);
    let graph = kit
        .builder("self")
        .add_node("m", kit.get(MAYBE))
        .connect_feedback("mm", port("m", "maybe"), port("m", "x"))
        .build();
    let mut rt = kit
        .load(graph, RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "m", "x", Val::U32(5)).await;
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "m"), NodePhase::Idle, "some(5) latched as 5");
    assert_eq!(
        rt.snapshot().await.unwrap().inputs[&id("m")][&"x".into()],
        "5"
    );
    rt.clear_input(&id("m"), &"x".into()).await.unwrap();
    settle(&mut rt).await;
    assert_eq!(phase(&rt, "m"), NodePhase::Idle);
    assert!(
        !rt.snapshot().await.unwrap().inputs.contains_key(&id("m")),
        "none latched as no value"
    );
}

#[tokio::test]
async fn tick_until_interrupts_an_endless_tick() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit
        .load(pipeline(&kit), RuntimeConfig::default(), Perf)
        .await;
    let tick = rt
        .tick_until(tokio::time::sleep(Duration::from_millis(100)))
        .await;
    assert!(matches!(tick, TickResult::Interrupted), "{tick:?}");
    assert_eq!(phase(&rt, "cons"), NodePhase::Running, "it carries on");
    rt.cancel(&id("cons")).unwrap();
}

#[tokio::test]
async fn re_injecting_the_current_value_keeps_a_loop_going() {
    let kit = Kit::new(&[RELAY]);
    let relay = kit.get(RELAY);
    let graph = kit
        .builder("loop")
        .add_node("a", relay.clone())
        .add_node("b", relay.clone())
        .add_node("c", relay)
        .connect("ab", port("a", "out"), port("b", "in"))
        .connect("bc", port("b", "out"), port("c", "in"))
        .connect_feedback("ba", port("b", "out"), port("a", "in"))
        .build();
    let config = RuntimeConfig {
        max_steps_per_tick: 2,
        ..RuntimeConfig::default()
    };
    let mut rt = kit.load(graph, config, Trace::new()).await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    assert!(matches!(rt.tick().await, TickResult::StepLimitReached));
    // Not a change: the feedback buffered for `a.in` stands.
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    for _ in 0..8 {
        let _ = rt.tick().await;
    }
    assert!(
        read_u32(&mut rt, "b", "out").await > 2,
        "the loop kept counting"
    );
}

#[tokio::test]
async fn a_restored_replay_keeps_the_host_write_over_stale_feedback() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER, RELAY]);
    let graph = || {
        kit.builder("stale")
            .add_node("prod", kit.get(STREAM_PRODUCER))
            .add_node("cons", kit.get(STREAM_CONSUMER))
            .add_node("r", kit.get(RELAY))
            .connect("s", port("prod", "items"), port("cons", "items"))
            .connect_feedback("t", port("cons", "total"), port("r", "in"))
            .build()
    };
    let mut rt = kit.load(graph(), small_slices(), Trace::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(500)).await;
    inject(&mut rt, "cons", "delay", Val::U32(4)).await;
    inject(&mut rt, "r", "in", Val::U32(1)).await;
    poll_tick_once(&mut rt).await;
    // Newer than what the in-flight generation will feed back.
    inject(&mut rt, "r", "in", Val::U32(100)).await;
    let snapshot = rt.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .islands
            .iter()
            .find(|i| i.running.is_some())
            .unwrap()
            .stale_feedback,
        [witgraph_ir::ConnectionId::from("t")].into_iter().collect()
    );

    for graph_rt in [&mut rt] {
        settle(graph_rt).await;
        assert_eq!(read_u32(graph_rt, "r", "out").await, 100, "original");
    }
    let mut fresh = kit.load(graph(), small_slices(), Trace::new()).await;
    fresh.restore(&snapshot).await.unwrap();
    settle(&mut fresh).await;
    assert_eq!(
        read_u32(&mut fresh, "r", "out").await,
        100,
        "restored replay"
    );

    // A host write after the restore wins over the replay's feedback too.
    let mut again = kit.load(graph(), small_slices(), Trace::new()).await;
    let mut clean = snapshot.clone();
    for island in &mut clean.islands {
        island.stale_feedback.clear();
    }
    again.restore(&clean).await.unwrap();
    inject(&mut again, "r", "in", Val::U32(200)).await;
    settle(&mut again).await;
    assert_eq!(read_u32(&mut again, "r", "out").await, 200);
}

#[tokio::test]
async fn interrupted_ticks_wait_for_every_feedback_source() {
    let kit = Kit::new(&[RELAY, STREAM_PRODUCER, STREAM_CONSUMER]);
    let graph = || {
        kit.builder("two-sources")
            .add_node("t", kit.get(RELAY))
            .add_node("s1", kit.get(RELAY))
            .add_node("prod", kit.get(STREAM_PRODUCER))
            .add_node("cons", kit.get(STREAM_CONSUMER))
            .connect("ts", port("t", "out"), port("s1", "in"))
            .connect("tc", port("t", "out"), port("cons", "take"))
            .connect("pc", port("prod", "items"), port("cons", "items"))
            .connect_feedback("st", port("s1", "out"), port("t", "in"))
            .connect_feedback("ct", port("cons", "count"), port("t", "add"))
            .build()
    };
    async fn seed(rt: &mut RuntimeGraph<Trace>) {
        inject(rt, "t", "in", Val::U32(1)).await;
        inject(rt, "s1", "add", Val::U32(1)).await;
        inject(rt, "cons", "delay", Val::U32(300)).await;
    }
    let mut full = kit.load(graph(), small_slices(), Trace::new()).await;
    seed(&mut full).await;
    let mut expected = Vec::new();
    for _ in 0..4 {
        let _ = full.tick().await;
        expected.push(read_u32(&mut full, "t", "out").await);
    }

    let mut cut = kit.load(graph(), small_slices(), Trace::new()).await;
    seed(&mut cut).await;
    let mut seen: Vec<u32> = Vec::new();
    for _ in 0..40 {
        let _ = cut
            .tick_until(tokio::time::sleep(Duration::from_millis(20)))
            .await;
        if let Some(Val::U32(out)) = read(&mut cut, "t", "out").await
            && seen.last() != Some(&out)
        {
            seen.push(out);
        }
        if seen.len() >= expected.len() {
            break;
        }
    }
    assert!(
        expected.starts_with(&seen) && !seen.is_empty(),
        "interrupted ticks follow full ticks: {seen:?} vs {expected:?}"
    );
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

async fn load_claimants(kit: &Kit, a: f64, b: f64) -> RuntimeGraph<Trace> {
    let mut rt = kit
        .load(
            two_claimants(kit, a, b),
            RuntimeConfig::default(),
            Trace::new(),
        )
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0)).await;
    inject(&mut rt, "b", "in", Val::Float64(2.0)).await;
    inject(&mut rt, "free", "in", Val::Float64(3.0)).await;
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
    assert_eq!(read_f64(&mut rt, "a", "out").await, 1.0);
    assert_eq!(read_f64(&mut rt, "b", "out").await, 2.0);
    assert_eq!(read_f64(&mut rt, "free", "out").await, 3.0);
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
async fn an_island_over_committing_a_resource_is_rejected() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut graph = pipeline(&kit);
    for node in &mut graph.nodes {
        node.resources
            .insert("disk".to_string().into(), ResourceClaim::new(0.6).unwrap());
    }
    let failure = graph
        .compile(&kit.contracts)
        .expect_err("the island sums to 1.2");
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| matches!(d, witgraph_ir::Diagnostic::IslandOverclaims { .. })),
        "{failure:?}"
    );
}

/// Hands out island data until `panic_after` Stores were made, then
/// panics.
struct PanickingData {
    made: std::sync::atomic::AtomicUsize,
    panic_after: usize,
}

impl Host for PanickingData {
    type Data = HostState;

    fn link(
        &self,
        _: &NodeId,
        _: &ComponentContract,
        _: &mut wasmtime::component::Linker<HostState>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }

    fn island_data(&self, _: &[NodeId], state: HostState) -> wasmtime::Result<HostState> {
        let made = self.made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(made < self.panic_after, "island data panics");
        Ok(state)
    }
}

#[tokio::test]
async fn a_panic_making_island_data_on_a_rebuild_is_a_restart_fault() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).unwrap();
    let host = PanickingData {
        made: Default::default(),
        panic_after: 1,
    };
    let mut rt =
        RuntimeGraph::load_with_host(compiled, &kit.wasm, RuntimeConfig::default(), Perf, host)
            .await
            .expect("loads");
    settle(&mut rt).await;
    rt.cancel(&id("e")).unwrap();
    rt.rerun(&id("e")).unwrap();
    settle(&mut rt).await;
    assert!(
        matches!(
            rt.node_state(&id("e")).unwrap().fault_cause(),
            Some(NodeFault::Restart { .. })
        ),
        "{:?}",
        rt.node_state(&id("e"))
    );
}

// ---- Loading ----

#[tokio::test]
async fn a_node_finds_the_component_prepared_against_its_contract() {
    use witgraph_runtime::graph::PreparedComponent;
    let kit = Kit::new(&[ECHO]);
    // Two revisions of one contract that differ only in a doc comment: one
    // id, content hash included.
    let documented = ComponentContract {
        docs: Some("Another revision.".into()),
        ..kit.contracts[0].clone()
    };
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let prepared = [&kit.contracts[0], &documented]
        .map(|contract| PreparedComponent::new(&engine, contract, ECHO.wasm).unwrap());
    for (contracts, order) in [
        (std::slice::from_ref(&documented), [0, 1]),
        (std::slice::from_ref(&documented), [1, 0]),
        (&kit.contracts[..], [1, 0]),
    ] {
        let compiled = Graph::builder("e")
            .add_component(&contracts[0])
            .add_node("e", contracts[0].id.clone())
            .build()
            .compile(contracts)
            .unwrap();
        let prepared = order.map(|i| prepared[i].clone());
        RuntimeGraph::load_prepared(
            compiled,
            &prepared,
            RuntimeConfig::default(),
            Perf,
            witgraph_runtime::NoCapabilities,
        )
        .await
        .unwrap_or_else(|e| panic!("{order:?}: {e}"));
    }
}

#[tokio::test]
async fn prepared_components_load_on_their_engine() {
    use witgraph_runtime::graph::PreparedComponent;
    let kit = Kit::new(&[ECHO]);
    let graph = || kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let contract = &kit.contracts[0];
    let prepared = PreparedComponent::new(&engine, contract, ECHO.wasm).unwrap();
    assert_eq!(prepared.contract(), contract);

    let compiled = graph().compile(&kit.contracts).unwrap();
    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        std::slice::from_ref(&prepared),
        RuntimeConfig::default(),
        Perf,
        witgraph_runtime::NoCapabilities,
    )
    .await
    .expect("the engine comes from the prepared component");
    assert!(wasmtime::Engine::same(rt.engine(), &engine));
    assert!(wasmtime::Engine::same(
        rt.config().engine.as_ref().unwrap(),
        &engine
    ));
    inject(&mut rt, "e", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    assert_eq!(read_f64(&mut rt, "e", "out").await, 1.0);

    let other = RuntimeConfig {
        engine: Some(RuntimeConfig::default().new_engine().unwrap()),
        ..RuntimeConfig::default()
    };
    let err = RuntimeGraph::load_prepared(
        graph().compile(&kit.contracts).unwrap(),
        std::slice::from_ref(&prepared),
        other,
        Perf,
        witgraph_runtime::NoCapabilities,
    )
    .await
    .err()
    .expect("prepared on another engine");
    assert!(
        matches!(err, LoadError::Runtime(RuntimeError::InvalidConfig { .. })),
        "{err}"
    );

    let err = RuntimeGraph::load_prepared(
        graph().compile(&kit.contracts).unwrap(),
        &[],
        RuntimeConfig::default(),
        Perf,
        witgraph_runtime::NoCapabilities,
    )
    .await
    .err()
    .expect("nothing prepared for `e`");
    assert!(matches!(err, LoadError::MissingWasm { .. }), "{err}");
}

#[tokio::test]
async fn a_component_prepared_against_another_contract_is_rejected() {
    use witgraph_runtime::graph::PreparedComponent;
    let kit = Kit::new(&[ECHO]);
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let real = kit.contracts[0].clone();
    // Same id, other ports: bytes checked against `real` must not be wired
    // to it.
    let mut other = real.clone();
    other.outputs.retain(|p| p.name.as_str() != "out");
    let prepared = PreparedComponent::new(&engine, &real, ECHO.wasm).unwrap();
    let graph = Graph::builder("e")
        .add_component(&other)
        .add_node("e", other.id.clone())
        .build();
    let compiled = graph.compile(std::slice::from_ref(&other)).unwrap();
    let err = RuntimeGraph::load_prepared(
        compiled,
        std::slice::from_ref(&prepared),
        RuntimeConfig::default(),
        Perf,
        witgraph_runtime::NoCapabilities,
    )
    .await
    .err()
    .expect("prepared against another contract");
    assert!(matches!(err, LoadError::ContractMismatch { .. }), "{err}");
}

#[tokio::test]
async fn an_engine_without_component_model_maps_is_rejected() {
    let kit = Kit::new(&[ECHO]);
    let mut config = wasmtime::Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .concurrency_support(true)
        .consume_fuel(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let graph = kit.builder("e").add_node("e", kit.get(ECHO)).build();
    let err = RuntimeGraph::load(
        graph.compile(&kit.contracts).unwrap(),
        &kit.wasm,
        RuntimeConfig {
            engine: Some(engine),
            ..RuntimeConfig::default()
        },
        Perf,
    )
    .await
    .err()
    .expect("the engine cannot load a world importing a map");
    assert!(
        matches!(err, LoadError::Runtime(RuntimeError::InvalidConfig { ref message }) if message.contains("map")),
        "{err}"
    );
}

#[tokio::test]
async fn bytes_of_another_component_are_rejected() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("mismatch").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let wasm = HashMap::from([(kit.get(ECHO), RELAY.wasm)]);
    let err = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf)
        .await
        .err()
        .expect("relay bytes do not implement echo");
    assert!(matches!(err, LoadError::ContractMismatch { .. }), "{err}");
}

#[tokio::test]
async fn missing_bytes_are_rejected() {
    let kit = Kit::new(&[ECHO]);
    let graph = kit.builder("missing").add_node("e", kit.get(ECHO)).build();
    let compiled = graph.compile(&kit.contracts).expect("compiles");
    let err = RuntimeGraph::load(compiled, &HashMap::new(), RuntimeConfig::default(), Perf)
        .await
        .err()
        .expect("no bytes");
    assert!(matches!(err, LoadError::MissingWasm { .. }), "{err}");
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
        .load(counter_loop(&kit), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::U32(0)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    for _ in 0..3 {
        assert!(matches!(rt.tick().await, TickResult::Progress));
    }
    assert_eq!(read_u32(&mut rt, "b", "out").await, 3);
    let snapshot = rt.snapshot().await.unwrap();

    let mut fresh = kit
        .load(counter_loop(&kit), RuntimeConfig::default(), Trace::new())
        .await;
    fresh.restore(&snapshot).await.expect("same graph");
    assert_eq!(
        read_u32(&mut fresh, "b", "out").await,
        3,
        "latched outputs come back"
    );
    assert!(matches!(fresh.tick().await, TickResult::Progress));
    assert_eq!(
        read_u32(&mut fresh, "b", "out").await,
        4,
        "the loop continues where it stopped"
    );
    let trace = fresh.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 1);
    assert_eq!(runs_of(&trace, "b"), 1);
    assert_eq!(
        fresh.snapshot().await.unwrap().outputs[&id("b")][&"out".into()],
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
        .load(graph(), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::Float64(1.0)).await;
    settle(&mut rt).await;
    inject(&mut rt, "b", "in", Val::Float64(2.0)).await;
    let snapshot = rt.snapshot().await.unwrap();
    assert!(snapshot.quiescent);
    assert_eq!(
        snapshot.islands,
        vec![IslandSnapshot {
            members: vec![id("b")],
            running: None,
            queued: true,
            stale_feedback: Default::default(),
        }]
    );
    assert_eq!(snapshot.phases[&id("a")], NodePhase::Idle);

    let mut fresh = kit
        .load(graph(), RuntimeConfig::default(), Trace::new())
        .await;
    fresh.restore(&snapshot).await.unwrap();
    settle(&mut fresh).await;
    let trace = fresh.mode().trace();
    assert_eq!(runs_of(&trace, "a"), 0, "a had no work outstanding");
    assert_eq!(runs_of(&trace, "b"), 1);
    assert_eq!(read_f64(&mut fresh, "a", "out").await, 1.0);
    assert_eq!(read_f64(&mut fresh, "b", "out").await, 2.0);
}

/// Polls one tick exactly once, leaving a long generation in flight.
async fn poll_tick_once<M: RuntimeMode, H: Host>(rt: &mut RuntimeGraph<M, H>) {
    let polled = tokio::time::timeout(Duration::ZERO, rt.tick()).await;
    assert!(polled.is_err(), "the generation outlives one poll");
}

fn small_slices() -> RuntimeConfig {
    RuntimeConfig {
        yield_interval: 10_000,
        ..RuntimeConfig::default()
    }
}

#[tokio::test]
async fn a_mid_flight_snapshot_replays_the_streaming_generation() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    // Endless producer; the consumer stops after 5000 items.
    inject(&mut rt, "cons", "take", Val::U32(5000)).await;
    inject(&mut rt, "cons", "delay", Val::U32(4)).await;
    poll_tick_once(&mut rt).await;
    assert_eq!(phase(&rt, "cons"), NodePhase::Running);

    let snapshot = rt.snapshot().await.unwrap();
    assert!(!snapshot.quiescent);
    assert_eq!(snapshot.phases[&id("cons")], NodePhase::Running);
    let [island] = snapshot.islands.as_slice() else {
        panic!("one island in flight: {:?}", snapshot.islands)
    };
    let started = island.running.as_ref().expect("in flight");
    assert_eq!(started[&id("cons")][&"take".into()], "5000");

    let err = rt.restore(&snapshot).await.unwrap_err();
    assert!(matches!(err, RuntimeError::NotQuiescent), "{err}");

    let mut fresh = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    fresh
        .restore(&snapshot)
        .await
        .expect("fresh graph has nothing in flight");
    settle(&mut fresh).await;
    settle(&mut rt).await;
    let expected = (0..5000).sum::<u32>();
    for graph in [&mut rt, &mut fresh] {
        assert_eq!(read_u32(graph, "cons", "count").await, 5000);
        assert_eq!(read_u32(graph, "cons", "total").await, expected);
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
    let mut rt = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(3000)).await;
    inject(&mut rt, "cons", "delay", Val::U32(4)).await;
    poll_tick_once(&mut rt).await;
    let snapshot = rt.snapshot().await.unwrap();

    rt.shutdown();
    rt.restore(&snapshot)
        .await
        .expect("nothing in flight after shutdown");
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "count").await, 3000);
    assert_eq!(
        phase(&rt, "cons"),
        NodePhase::Idle,
        "the stopped island was rebuilt"
    );
}

#[tokio::test]
async fn restore_works_right_after_cancel() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(3000)).await;
    inject(&mut rt, "cons", "delay", Val::U32(4)).await;
    poll_tick_once(&mut rt).await;
    let snapshot = rt.snapshot().await.unwrap();
    assert!(!snapshot.quiescent);

    rt.cancel(&id("cons")).unwrap();
    rt.restore(&snapshot)
        .await
        .expect("a cancelled island has nothing in flight");
    settle(&mut rt).await;
    assert_eq!(read_u32(&mut rt, "cons", "count").await, 3000);
    assert_eq!(phase(&rt, "cons"), NodePhase::Idle);
}

#[tokio::test]
async fn restore_rejects_another_graph_and_bad_values() {
    let kit = Kit::new(&[ECHO, CONFIGURABLE]);
    let echo_graph = || kit.builder("e").add_node("n", kit.get(ECHO)).build();
    let mut rt = kit
        .load(echo_graph(), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "n", "in", Val::Float64(1.5)).await;
    settle(&mut rt).await;
    let snapshot = rt.snapshot().await.unwrap();

    let other = kit
        .builder("c")
        .add_node("n", kit.get(CONFIGURABLE))
        .build();
    let mut wrong = kit
        .load(other, RuntimeConfig::default(), Trace::new())
        .await;
    let err = wrong.restore(&snapshot).await.unwrap_err();
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
        .load(echo_graph(), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut fresh, "n", "in", Val::Float64(7.0)).await;
    settle(&mut fresh).await;
    let before = fresh.snapshot().await.unwrap();
    let err = fresh.restore(&bad).await.unwrap_err();
    assert!(matches!(err, RuntimeError::SnapshotValue { .. }), "{err}");

    let mut unknown = snapshot.clone();
    unknown
        .outputs
        .get_mut(&id("n"))
        .unwrap()
        .insert("nope".into(), "1".into());
    let err = fresh.restore(&unknown).await.unwrap_err();
    assert!(matches!(err, RuntimeError::SnapshotValue { .. }), "{err}");
    assert_eq!(
        fresh.snapshot().await.unwrap(),
        before,
        "a failed restore changes nothing"
    );
    assert_eq!(read_f64(&mut fresh, "n", "out").await, 7.0);
}

#[tokio::test]
async fn restore_rejects_a_rewired_graph() {
    let kit = Kit::new(&[ECHO]);
    let wired = |from: &str| {
        kit.builder("rewired")
            .add_node("e1", kit.get(ECHO))
            .add_node("e2", kit.get(ECHO))
            .add_node("e3", kit.get(ECHO))
            .connect("c", port(from, "out"), port("e2", "in"))
            .build()
    };
    let mut rt = kit
        .load(wired("e1"), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "e1", "in", Val::Float64(4.0)).await;
    settle(&mut rt).await;
    let snapshot = rt.snapshot().await.unwrap();

    let mut other = kit
        .load(wired("e3"), RuntimeConfig::default(), Trace::new())
        .await;
    let err = other.restore(&snapshot).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::SnapshotMismatch { .. }),
        "same nodes, other wiring: {err}"
    );
}

#[tokio::test]
async fn restore_matches_islands_by_their_members_in_any_order() {
    let kit = Kit::new(&[STREAM_PRODUCER, STREAM_CONSUMER]);
    let mut rt = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    inject(&mut rt, "cons", "take", Val::U32(300)).await;
    inject(&mut rt, "cons", "delay", Val::U32(4)).await;
    poll_tick_once(&mut rt).await;
    let mut snapshot = rt.snapshot().await.unwrap();
    assert_eq!(
        snapshot.islands[0].members,
        vec![id("cons"), id("prod")],
        "sorted"
    );
    snapshot.islands[0].members.reverse();

    let mut fresh = kit.load(pipeline(&kit), small_slices(), Trace::new()).await;
    fresh
        .restore(&snapshot)
        .await
        .expect("members match as a set");
    settle(&mut fresh).await;
    assert_eq!(read_u32(&mut fresh, "cons", "count").await, 300);
}

#[tokio::test]
async fn restore_rejects_a_replay_missing_a_required_input() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = kit
        .load(counter_loop(&kit), RuntimeConfig::default(), Trace::new())
        .await;
    let mut snapshot = rt.snapshot().await.unwrap();
    snapshot.islands = vec![IslandSnapshot {
        members: vec![id("a")],
        running: Some([(id("a"), Default::default())].into_iter().collect()),
        queued: false,
        stale_feedback: Default::default(),
    }];
    let err = rt.restore(&snapshot).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::SnapshotValue { ref port, .. } if port.as_str() == "in"),
        "`a.in` is required: {err}"
    );

    snapshot.islands[0].running = Some(
        [(
            id("a"),
            [
                ("in".into(), "1".to_string()),
                ("out".into(), "1".to_string()),
            ]
            .into_iter()
            .collect(),
        )]
        .into_iter()
        .collect(),
    );
    let err = rt.restore(&snapshot).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::SnapshotValue { ref port, .. } if port.as_str() == "out"),
        "`out` is not an input: {err}"
    );
}

#[tokio::test]
async fn restore_rejects_inconsistent_snapshots_and_changes_nothing() {
    let kit = Kit::new(&[RELAY]);
    let mut rt = kit
        .load(counter_loop(&kit), RuntimeConfig::default(), Trace::new())
        .await;
    inject(&mut rt, "a", "in", Val::U32(1)).await;
    inject(&mut rt, "a", "add", Val::U32(1)).await;
    inject(&mut rt, "b", "add", Val::U32(1)).await;
    // A feedback loop never settles: a few iterations will do.
    for _ in 0..3 {
        let _ = rt.tick().await;
    }
    let good = rt.snapshot().await.unwrap();
    let running = |node: &str| -> Option<witgraph_runtime::PortValues> {
        Some(
            [(
                id(node),
                [("in".into(), "1".to_string())].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
        )
    };
    let island = |members: &[&str], running, stale: &[&str]| IslandSnapshot {
        members: members.iter().map(|m| id(m)).collect(),
        running,
        queued: false,
        stale_feedback: stale.iter().map(|c| (*c).into()).collect(),
    };
    let mut cases: Vec<(&str, Snapshot)> = Vec::new();
    let mut twice = good.clone();
    twice.islands = vec![island(&["a"], None, &[]), island(&["a"], None, &[])];
    cases.push(("an island listed twice", twice));
    let mut unknown = good.clone();
    unknown.islands = vec![island(&["a", "b"], None, &[])];
    cases.push(("no such island", unknown));
    let mut outside = good.clone();
    outside.islands = vec![island(&["a"], running("b"), &[])];
    cases.push(("a member of another island", outside));
    let mut not_feedback = good.clone();
    not_feedback.feedback.insert("ab".into(), "1".into());
    cases.push(("a non-feedback connection", not_feedback));
    let mut stale_elsewhere = good.clone();
    stale_elsewhere.islands = vec![island(&["a"], running("a"), &["ba"])];
    cases.push(("stale feedback from another island", stale_elsewhere));
    let mut stale_idle = good.clone();
    stale_idle.islands = vec![island(&["b"], None, &["ba"])];
    cases.push(("stale feedback without a replay", stale_idle));
    let mut phases = good.clone();
    phases.phases.insert(id("ghost"), NodePhase::Idle);
    cases.push(("a phase for a node not in the graph", phases));

    let before = rt.snapshot().await.unwrap();
    for (what, snapshot) in cases {
        let err = rt.restore(&snapshot).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::SnapshotMismatch { .. }),
            "{what}: {err}"
        );
        assert_eq!(
            rt.snapshot().await.unwrap(),
            before,
            "{what}: nothing changed"
        );
    }
    rt.restore(&good).await.expect("the good snapshot restores");
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
        connections: vec![witgraph_ir::Connection {
            id: "ba".into(),
            from: port("n", "out"),
            to: port("n", "in"),
            feedback: true,
            attrs: Default::default(),
        }],
        links: Vec::new(),
        quiescent: false,
        phases: [(id("n"), NodePhase::Running)].into_iter().collect(),
        inputs: values("in", "1.5"),
        outputs: Default::default(),
        feedback: [("ba".into(), "3".to_string())].into_iter().collect(),
        islands: vec![IslandSnapshot {
            members: vec![id("n")],
            running: Some(values("in", "1.0")),
            queued: true,
            stale_feedback: Default::default(),
        }],
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let back: Snapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(snapshot, back);
}

#[test]
fn a_misspelled_snapshot_key_is_rejected() {
    let json = r#"{"nodes":{},"connections":[],"quiescent":true,"phases":{},"inputs":{},"outputs":{},"feedback":{},"islands":[{"members":["n"],"runing":null,"queued":false}]}"#;
    assert!(
        serde_json::from_str::<Snapshot>(json).is_err(),
        "a typo must not silently drop a replay"
    );
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
