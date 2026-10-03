//! In-crate tests of the scheduler's internals.

use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};

use test_components::{BUSY_LOOP, Guest, RELAY, STREAM_CONSUMER, STREAM_PRODUCER};
use witgraph_ir::{ComponentContract, Graph, GraphBuilder};

use super::load::semver_compatible;
use super::*;
use crate::engine::{HostState, IslandEventKind};
use crate::error::NodeFault;
use crate::schedule::TickResult;

/// Loads a graph over `guests`; `build` adds nodes and connections to a
/// builder that already has every guest's component in its table.
async fn load(
    guests: &[Guest],
    build: impl FnOnce(GraphBuilder, &[ComponentRef]) -> Graph,
) -> RuntimeGraph {
    let mut contracts = Vec::new();
    let mut wasm = HashMap::new();
    for guest in guests {
        let mut lowered = witgraph_wit::load_components(guest.wit).unwrap();
        let contract = lowered.remove(0);
        wasm.insert(contract.id.clone(), guest.wasm);
        contracts.push(contract);
    }
    let ids: Vec<ComponentRef> = contracts.iter().map(|c| c.id.clone()).collect();
    let builder = contracts
        .iter()
        .fold(Graph::builder("t"), |b, c| b.add_component(c));
    let compiled = build(builder, &ids).compile(&contracts).unwrap();
    RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf)
        .await
        .unwrap()
}

fn output<M: RuntimeMode, H: Host>(rt: &RuntimeGraph<M, H>, node: &str, port: &str) -> Option<Val> {
    rt.read_output(&node.into(), &port.into()).unwrap()
}

#[test]
fn semver_compatibility_follows_wasmtime() {
    assert!(semver_compatible("a:b/c@0.1.0", "a:b/c@0.1.3"));
    assert!(semver_compatible("a:b/c@1.2.0", "a:b/c@1.9.1"));
    assert!(!semver_compatible("a:b/c@0.1.0", "a:b/c@0.2.0"));
    assert!(!semver_compatible("a:b/c@1.0.0", "a:b/c@2.0.0"));
    assert!(!semver_compatible("a:b/c@0.0.1", "a:b/c@0.0.2"));
    assert!(!semver_compatible("a:b/c@0.1.0", "a:b/d@0.1.0"));
    assert!(
        !semver_compatible("config", "config"),
        "no version, no match"
    );
}

#[tokio::test]
async fn cancel_handles_events_a_dropped_tick_left_behind() {
    let mut rt = load(&[STREAM_PRODUCER, STREAM_CONSUMER, RELAY], |b, ids| {
        b.add_node("prod", ids[0].clone())
            .add_node("cons", ids[1].clone())
            .add_node("r", ids[2].clone())
            .connect(
                "s",
                PortRef::new("prod", "items"),
                PortRef::new("cons", "items"),
            )
            .connect("t", PortRef::new("cons", "total"), PortRef::new("r", "in"))
            .build()
    })
    .await;
    // An endless generation is in flight when the tick is dropped.
    let dropped = tokio::time::timeout(Duration::from_millis(50), rt.tick()).await;
    assert!(dropped.is_err());
    let cons = NodeId::from("cons");
    let island = rt.compiled.island_of(&cons).unwrap();
    let generation = rt.slots[island].state.running_generation().unwrap();
    let member = rt.slots[island]
        .plan
        .members
        .iter()
        .position(|m| m.node == cons)
        .unwrap();
    // As if `cons` returned during the dropped tick's last poll.
    rt.events_tx
        .unbounded_send(IslandEvent {
            island,
            generation,
            kind: IslandEventKind::RunReturned {
                member,
                values: vec![("total".into(), Val::U32(5)), ("count".into(), Val::U32(2))],
            },
        })
        .unwrap();

    rt.cancel(&cons).unwrap();
    assert_eq!(output(&rt, "cons", "total"), Some(Val::U32(5)));
    let tick = tokio::time::timeout(Duration::from_secs(30), rt.tick())
        .await
        .unwrap();
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    assert_eq!(
        output(&rt, "r", "out"),
        Some(Val::U32(5)),
        "the returned run's outputs were delivered"
    );
}

/// Fails every island Store's data while `fail` is set.
struct Flaky {
    fail: Arc<AtomicBool>,
}

impl Host for Flaky {
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
        if self.fail.load(Ordering::SeqCst) {
            return Err(wasmtime::format_err!("no data"));
        }
        Ok(state)
    }
}

/// Ticks until the graph is idle, failing (not hanging) the test if a
/// tick does not end, aborts, or the graph never settles.
async fn settle<M: RuntimeMode, H: Host>(rt: &mut RuntimeGraph<M, H>) {
    for _ in 0..20 {
        let tick = tokio::time::timeout(Duration::from_secs(20), rt.tick())
            .await
            .expect("the tick ends");
        match tick {
            TickResult::Idle => return,
            TickResult::Progress => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("the graph never settled");
}

#[tokio::test]
async fn work_owed_during_a_failing_rebuild_survives_it() {
    let contracts = vec![
        witgraph_wit::load_components(BUSY_LOOP.wit)
            .unwrap()
            .remove(0),
    ];
    let wasm: HashMap<ComponentRef, &[u8]> =
        HashMap::from([(contracts[0].id.clone(), BUSY_LOOP.wasm)]);
    let graph = Graph::builder("t")
        .add_component(&contracts[0])
        .add_node("busy", contracts[0].id.clone())
        .build();
    let fail = Arc::new(AtomicBool::new(false));
    let mut rt = RuntimeGraph::load_with_host(
        graph.compile(&contracts).unwrap(),
        &wasm,
        RuntimeConfig::default(),
        Perf,
        Flaky { fail: fail.clone() },
    )
    .await
    .unwrap();
    settle(&mut rt).await;
    let busy = NodeId::from("busy");
    rt.cancel(&busy).unwrap();
    fail.store(true, Ordering::SeqCst);
    rt.inject(&busy, &"action".into(), Val::Enum("spin".into()))
        .unwrap();
    // The rebuild starts, then an input changes before it fails.
    let index = rt.compiled.island_of(&busy).unwrap();
    rt.start_generation(index);
    rt.inject(&busy, &"action".into(), Val::Enum("finish".into()))
        .unwrap();
    // The attempt fails; the change it never saw is still owed, so the
    // island is rebuilt again (successfully, now) and runs on it.
    fail.store(false, Ordering::SeqCst);
    settle(&mut rt).await;
    let faults = rt.take_faults();
    assert!(
        matches!(faults.as_slice(), [report] if matches!(report.fault, NodeFault::Restart { .. })),
        "the first rebuild failed: {faults:?}"
    );
    assert_eq!(
        rt.node_state(&busy).unwrap().phase(),
        NodePhase::Idle,
        "and the island ran after it"
    );
}

#[tokio::test]
async fn a_failed_rebuild_frees_downstream_in_the_same_tick() {
    let contracts: Vec<ComponentContract> = [BUSY_LOOP, RELAY]
        .iter()
        .map(|g| witgraph_wit::load_components(g.wit).unwrap().remove(0))
        .collect();
    let wasm: HashMap<ComponentRef, &[u8]> = contracts
        .iter()
        .zip([BUSY_LOOP, RELAY])
        .map(|(c, g)| (c.id.clone(), g.wasm))
        .collect();
    let graph = contracts
        .iter()
        .fold(Graph::builder("t"), |b, c| b.add_component(c))
        .add_node("busy", contracts[0].id.clone())
        .add_node("r", contracts[1].id.clone())
        .connect("d", PortRef::new("busy", "done"), PortRef::new("r", "in"))
        .build();
    let fail = Arc::new(AtomicBool::new(false));
    let mut rt = RuntimeGraph::load_with_host(
        graph.compile(&contracts).unwrap(),
        &wasm,
        RuntimeConfig::default(),
        Perf,
        Flaky { fail: fail.clone() },
    )
    .await
    .unwrap();
    settle(&mut rt).await;
    assert_eq!(output(&rt, "r", "out"), Some(Val::U32(1)));

    let busy = NodeId::from("busy");
    fail.store(true, Ordering::SeqCst);
    rt.cancel(&busy).unwrap();
    rt.inject(&busy, &"action".into(), Val::Enum("finish".into()))
        .unwrap();
    rt.inject(&"r".into(), &"add".into(), Val::U32(5)).unwrap();
    // `busy` comes first and holds `r` back until its rebuild (the
    // start of its generation) fails; then `r` runs in the same tick.
    rt.config.max_steps_per_tick = 2;

    let tick = rt.tick().await;
    assert!(matches!(tick, TickResult::Progress), "{tick:?}");
    assert_eq!(output(&rt, "r", "out"), Some(Val::U32(6)));
    assert!(matches!(
        rt.node_state(&busy).unwrap().fault_cause(),
        Some(NodeFault::Restart { message }) if message.contains("no data")
    ));
}
