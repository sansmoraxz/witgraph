//! Holding node runs before they start, on wasmtime: the node breakpoints and stepping of a
//! debugger, as `beforeRun` gives them on a JavaScript host.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::time::Duration;

use test_components::ECHO;
use witgraph_ir::{ComponentRef, Graph, NodeId, PortName, PortRef};
use witgraph_runtime::{
    HoldAt, NodeHolds, OutputReader, Perf, RunningGraph, RuntimeConfig, RuntimeGraph, TickResult,
};

/// `a` feeding `b`, both `echo`.
async fn a_feeds_b() -> RuntimeGraph<Perf> {
    let contract = witgraph_wit::load_components(ECHO.wit).unwrap().remove(0);
    let id: ComponentRef = contract.id.clone();
    let graph = Graph::builder("holds")
        .add_component(&contract)
        .add_node("a", id.clone())
        .add_node("b", id.clone())
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let compiled = graph.compile(&[contract]).expect("graph compiles");
    let wasm = HashMap::from([(id, ECHO.wasm)]);
    RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf)
        .await
        .expect("graph loads")
}

fn nodes(names: &[&str]) -> HoldAt {
    HoldAt::Nodes(
        names
            .iter()
            .map(|name| NodeId::from(*name))
            .collect::<BTreeSet<_>>(),
    )
}

async fn inject(rt: &mut RuntimeGraph<Perf>, node: &str, wave: &str) {
    rt.inject_wave(&NodeId::from(node), &PortName::from("in"), wave)
        .await
        .unwrap();
}

/// Ticks until the graph is idle while `body` runs, with a limit on both.
async fn settling<T>(rt: &mut RuntimeGraph<Perf>, body: impl Future<Output = T>) -> T {
    let ticking = async {
        loop {
            match rt.tick().await {
                TickResult::Idle => break,
                TickResult::Progress | TickResult::StepLimitReached => {}
                other => panic!("a tick went wrong: {other:?}"),
            }
        }
    };
    let both = async { tokio::join!(ticking, body).1 };
    tokio::time::timeout(Duration::from_secs(30), both)
        .await
        .expect("it never finished")
}

#[tokio::test]
async fn a_held_node_waits_to_run_until_let_go_and_the_rest_runs() {
    let mut rt = a_feeds_b().await;
    let (holds, reader) = (rt.holds(), rt.reader());
    holds.hold_at(nodes(&["b"]));
    inject(&mut rt, "a", "5").await;
    let output = |node: &str| {
        let reader = reader.clone();
        let node = NodeId::from(node);
        async move {
            reader
                .read_output_wave(&node, &PortName::from("out"))
                .await
                .unwrap()
        }
    };
    settling(&mut rt, async {
        let held = holds.next_held().await;
        assert_eq!(held.node, NodeId::from("b"));
        assert_eq!(holds.held(), vec![held.clone()]);
        assert_eq!(output("a").await.as_deref(), Some("5"), "`a` was not held");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(output("b").await, None, "held for as long as it takes");
        holds.resume(held.id);
        assert!(holds.held().is_empty());
    })
    .await;
    assert_eq!(output("b").await.as_deref(), Some("5"));
}

#[tokio::test]
async fn what_a_held_run_will_run_on_can_be_read() {
    let mut rt = a_feeds_b().await;
    let (holds, reader) = (rt.holds(), rt.reader());
    holds.hold_at(nodes(&["b"]));
    inject(&mut rt, "a", "6").await;
    let input = |node: &str, port: &str| {
        let reader = reader.clone();
        let (node, port) = (NodeId::from(node), PortName::from(port));
        async move { reader.read_input_wave(&node, &port).await }
    };
    settling(&mut rt, async {
        let held = holds.next_held().await;
        // Fed by `a`, and written by the host.
        assert_eq!(input("b", "in").await.unwrap().as_deref(), Some("6"));
        assert_eq!(input("a", "in").await.unwrap().as_deref(), Some("6"));
        assert!(matches!(
            input("b", "out").await,
            Err(witgraph_runtime::RuntimeError::NotAValuePort { .. })
        ));
        assert!(matches!(
            input("c", "in").await,
            Err(witgraph_runtime::RuntimeError::UnknownNode { .. })
        ));
        holds.resume(held.id);
    })
    .await;
}

#[tokio::test]
async fn holding_every_node_holds_each_in_turn() {
    let mut rt = a_feeds_b().await;
    let holds = rt.holds();
    holds.hold_at(HoldAt::All);
    inject(&mut rt, "a", "1").await;
    let order = settling(&mut rt, async {
        let mut order = Vec::new();
        for _ in 0..2 {
            let held = holds.next_held().await;
            order.push(held.node.to_string());
            holds.resume(held.id);
        }
        order
    })
    .await;
    assert_eq!(order, ["a", "b"]);
}

#[tokio::test]
async fn releasing_lets_go_of_what_is_held_and_holds_nothing_after() {
    let mut rt = a_feeds_b().await;
    let holds = rt.holds();
    holds.hold_at(HoldAt::All);
    inject(&mut rt, "a", "2").await;
    settling(&mut rt, async {
        holds.next_held().await;
        holds.release();
    })
    .await;
    assert!(holds.held().is_empty());
    // Nothing is held now: this settles by itself.
    inject(&mut rt, "a", "3").await;
    settling(&mut rt, async {}).await;
    let out = rt
        .read_output_wave(&NodeId::from("b"), &PortName::from("out"))
        .await
        .unwrap();
    assert_eq!(out.as_deref(), Some("3"));
}

#[tokio::test]
async fn a_held_run_is_dropped_with_the_instance_a_shutdown_ends() {
    let mut rt = a_feeds_b().await;
    let holds = rt.holds();
    holds.hold_at(nodes(&["a"]));
    inject(&mut rt, "a", "4").await;
    // The tick waits on the held run: it is dropped once the hold is seen.
    let watching = holds.clone();
    let stop = async move {
        watching.next_held().await;
    };
    let result = rt.tick_until(stop).await;
    assert!(matches!(result, TickResult::Interrupted), "{result:?}");
    assert_eq!(holds.held().len(), 1);
    rt.shutdown();
    // The instance goes when the scheduler lets go of it, which a tick does.
    holds.hold_at(HoldAt::default());
    rt.rerun(&NodeId::from("a")).unwrap();
    settling(&mut rt, async {}).await;
    assert!(holds.held().is_empty(), "it went with its instance");
}
