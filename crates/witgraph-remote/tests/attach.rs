//! A debugger attaching over WebTransport, as the editor does from a browser.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::time::Duration;

use test_components::ECHO;
use witgraph_ir::{ComponentRef, Graph, NodeId, PortName, PortRef};
use witgraph_remote::{Debuggee, Served, Watched, serve};
use witgraph_runtime::{Perf, RunningGraph, RuntimeConfig, RuntimeGraph};

mod client {
    #![allow(missing_docs, clippy::all)]
    wit_bindgen_wrpc::generate!({
        world: "debugger",
        path: "wit",
        generate_all,
    });
}

use client::witgraph::debug::{graph, holds};

/// `a` feeding `b`, both `echo`, served with a host ticking it.
async fn attached() -> (Debuggee<Perf, witgraph_runtime::NoCapabilities>, Served) {
    let contract = witgraph_wit::load_components(ECHO.wit).unwrap().remove(0);
    let id: ComponentRef = contract.id.clone();
    let graph = Graph::builder("attach")
        .add_component(&contract)
        .add_node("a", id.clone())
        .add_node("b", id.clone())
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let compiled = graph.compile(&[contract]).unwrap();
    let wasm = HashMap::from([(id, ECHO.wasm)]);
    let rt = RuntimeGraph::load(
        compiled,
        &wasm,
        RuntimeConfig::default(),
        Watched::new(Perf),
    )
    .await
    .unwrap();
    let debuggee = Debuggee::new(rt).unwrap();
    let served = serve(
        debuggee.clone(),
        "127.0.0.1:0".parse().unwrap(),
        "secret".into(),
    )
    .await
    .unwrap();
    // The program's own loop, ticking as it would.
    let ticking = debuggee.clone();
    tokio::spawn(async move {
        loop {
            let _ = ticking.tick().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    (debuggee, served)
}

async fn connect(served: &Served, token: &str) -> anyhow::Result<wrpc_webtransport::Client> {
    let client = web_transport_quinn::ClientBuilder::new()
        .with_server_certificate_hashes(vec![served.certificate_hash.to_vec()])?;
    let mut url = url::Url::parse(&served.url)?;
    url.set_query(Some(&format!("token={token}")));
    let session = client.connect(url).await?;
    Ok(wrpc_webtransport::Client::from(session))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_without_the_token_is_refused() {
    let (_debuggee, served) = attached().await;
    assert!(connect(&served, "wrong").await.is_err());
    assert!(connect(&served, "secret").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_holds_a_node_and_sees_in_while_it_is_held() {
    let (debuggee, served) = attached().await;
    let wrpc = connect(&served, "secret").await.unwrap();

    let described = graph::describe(&wrpc, ()).await.unwrap();
    assert!(
        described.contains("\"a\"") && described.contains("\"b\""),
        "{described}"
    );
    let contracts = graph::contracts(&wrpc, ()).await.unwrap();
    assert!(
        contracts.contains("\"inputs\"") && contracts.contains("\"b\""),
        "{contracts}"
    );

    holds::hold_at(&wrpc, (), &holds::Holding::Nodes(vec!["b".into()]))
        .await
        .unwrap();
    graph::inject(&wrpc, (), "a", "in", "5")
        .await
        .unwrap()
        .unwrap();
    let held = tokio::time::timeout(Duration::from_secs(20), holds::next_held(&wrpc, ()))
        .await
        .expect("`b` is held")
        .unwrap();
    assert_eq!(held.node, "b");

    // The tick cannot end while `b` is held: what is asked of the graph still is answered.
    let state = tokio::time::timeout(Duration::from_secs(20), graph::node_state(&wrpc, (), "b"))
        .await
        .expect("answered while the tick is held")
        .unwrap()
        .unwrap();
    assert_eq!(state.phase, "running");
    // What it will run on, and what came before.
    let input = graph::read_input(&wrpc, (), "b", "in")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(input.as_deref(), Some("5"));
    let trace = graph::take_trace(&wrpc, ()).await.unwrap();
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, graph::TraceEvent::RunReturned(node) if node == "a")),
        "{trace:?}"
    );
    assert!(
        trace.iter().any(|event| matches!(
            event,
            graph::TraceEvent::PhaseTransition(graph::Transition { node, after, .. })
                if node == "b" && after == "running"
        )),
        "{trace:?}"
    );
    assert!(
        graph::read_input(&wrpc, (), "b", "out")
            .await
            .unwrap()
            .is_err()
    );
    let a = graph::read_output(&wrpc, (), "a", "out")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.as_deref(), Some("5"));
    // From the run every node owes at load, before anything was written.
    let before = graph::read_output(&wrpc, (), "b", "out")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(before.as_deref(), Some("5"), "`b` has not run on it");
    graph::rerun(&wrpc, (), "a").await.unwrap().unwrap();

    holds::resume(&wrpc, (), held.id).await.unwrap();
    let b = async {
        loop {
            let read = graph::read_output(&wrpc, (), "b", "out")
                .await
                .unwrap()
                .unwrap();
            if read.as_deref() == Some("5") {
                return read;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let b = tokio::time::timeout(Duration::from_secs(20), b)
        .await
        .expect("`b` ran");
    assert_eq!(b.as_deref(), Some("5"));
    // The host's own calls go through the same way.
    let state = debuggee
        .with(|graph| {
            Box::pin(async move { graph.node_state(&NodeId::from("a")).unwrap().phase() })
        })
        .await;
    let _ = (state, PortName::from("in"));
}

/// `counter` feeding itself and `walker`, a guest built with debug info, served with a host
/// ticking it; a debugger attached to it; and the line of `walker`'s source marked
/// `walk-first`, in its file. The debugger is the test's to keep: one that goes clears the
/// breakpoints set.
async fn lines_attached() -> (Served, wrpc_webtransport::Client, String, u32) {
    use test_components::{LINE_WALK, RELAY};
    use witgraph_runtime::OwnLines;

    let mut contracts = Vec::new();
    let mut wasm = HashMap::new();
    for guest in [RELAY, LINE_WALK] {
        let contract = witgraph_wit::load_components(guest.wit).unwrap().remove(0);
        wasm.insert(contract.id.clone(), guest.wasm);
        contracts.push(contract);
    }
    let graph = Graph::builder("lines")
        .add_component(&contracts[0])
        .add_component(&contracts[1])
        .add_node("counter", contracts[0].id.clone())
        .add_node("walker", contracts[1].id.clone())
        .connect_feedback(
            "loop",
            PortRef::new("counter", "out"),
            PortRef::new("counter", "in"),
        )
        .connect(
            "walk",
            PortRef::new("counter", "out"),
            PortRef::new("walker", "in"),
        )
        .build();
    let config = RuntimeConfig {
        debug_lines: Some(OwnLines::default()),
        ..RuntimeConfig::default()
    };
    let mut rt = RuntimeGraph::load(
        graph.compile(&contracts).unwrap(),
        &wasm,
        config,
        Watched::new(Perf),
    )
    .await
    .unwrap();
    rt.inject_wave(&NodeId::from("counter"), &PortName::from("in"), "0")
        .await
        .unwrap();
    rt.inject_wave(&NodeId::from("counter"), &PortName::from("add"), "1")
        .await
        .unwrap();
    let debuggee = Debuggee::new(rt).unwrap();
    let served = serve(
        debuggee.clone(),
        "127.0.0.1:0".parse().unwrap(),
        "secret".into(),
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        loop {
            let _ = debuggee.tick().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let wrpc = connect(&served, "secret").await.unwrap();
    let files = client::witgraph::debug::lines::files(&wrpc, ())
        .await
        .unwrap();
    let file = files
        .iter()
        .find(|file| file.ends_with("line-walk/src/lib.rs"))
        .unwrap()
        .clone();
    let text = client::witgraph::debug::lines::source(&wrpc, (), &file)
        .await
        .unwrap()
        .unwrap();
    let line = text
        .lines()
        .position(|line| line.ends_with("// mark: walk-first"))
        .unwrap()
        + 1;
    (served, wrpc, file, u32::try_from(line).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_stops_a_guest_on_a_line_and_reads_its_source() {
    use client::witgraph::debug::lines;

    let (served, wrpc, file, line) = lines_attached().await;
    let files = lines::files(&wrpc, ()).await.unwrap();
    assert_eq!(files.len(), 2, "`lib.rs` and `scale.rs`: {files:?}");
    assert_eq!(
        lines::files_of(&wrpc, (), "walker").await.unwrap(),
        files,
        "the walker's guest has them all"
    );
    assert!(
        lines::files_of(&wrpc, (), "counter")
            .await
            .unwrap()
            .is_empty(),
        "the counter's has no debug info"
    );
    assert!(
        lines::source(&wrpc, (), "/etc/passwd")
            .await
            .unwrap()
            .is_none(),
        "only its own"
    );

    // As the editor does: asking for states, again and again, while the program ticks.
    let polling = connect(&served, "secret").await.unwrap();
    tokio::spawn(async move {
        loop {
            let _ = graph::node_state(&polling, (), "walker").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    assert_eq!(
        lines::set_breakpoint(&wrpc, (), &lines::BreakpointScope::EveryNode, &file, line)
            .await
            .unwrap(),
        Some(line)
    );
    let stop = tokio::time::timeout(Duration::from_secs(20), lines::next_stop(&wrpc, ()))
        .await
        .expect("it stops")
        .unwrap();
    assert_eq!((stop.line, stop.node.as_deref()), (line, Some("walker")));
    assert!(
        stop.variables.iter().any(|variable| variable.name == "x"),
        "{:?}",
        stop.variables
    );
    lines::clear_breakpoint(&wrpc, (), &lines::BreakpointScope::EveryNode, &file, line)
        .await
        .unwrap();
    lines::resume(&wrpc, ()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_that_goes_lets_go_of_what_it_held() {
    let (_debuggee, served) = attached().await;
    let gone = connect(&served, "secret").await.unwrap();
    holds::hold_at(&gone, (), &holds::Holding::Nodes(vec!["b".into()]))
        .await
        .unwrap();
    graph::inject(&gone, (), "a", "in", "7")
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), holds::next_held(&gone, ()))
        .await
        .expect("`b` is held")
        .unwrap();
    // Gone without a word, as a closed tab goes.
    drop(gone);

    let next = connect(&served, "secret").await.unwrap();
    let b = async {
        loop {
            let read = graph::read_output(&next, (), "b", "out")
                .await
                .unwrap()
                .unwrap();
            if read.as_deref() == Some("7") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), b)
        .await
        .expect("`b` was let go, and ran");
    assert!(holds::held(&next, ()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_that_goes_leaves_no_breakpoint_behind() {
    use client::witgraph::debug::lines;

    let (served, gone, file, line) = lines_attached().await;
    assert_eq!(
        lines::set_breakpoint(&gone, (), &lines::BreakpointScope::EveryNode, &file, line)
            .await
            .unwrap(),
        Some(line)
    );
    tokio::time::timeout(Duration::from_secs(20), lines::next_stop(&gone, ()))
        .await
        .expect("it stops")
        .unwrap();
    // Gone without clearing it, as a closed tab goes.
    drop(gone);

    // The guest is let go, and its next runs stop nowhere: it goes on walking.
    let next = connect(&served, "secret").await.unwrap();
    let walked = async {
        let mut seen = Vec::new();
        while seen.len() < 3 {
            let out = graph::read_output(&next, (), "walker", "out")
                .await
                .unwrap()
                .unwrap();
            if seen.last() != Some(&out) {
                seen.push(out);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), walked)
        .await
        .expect("the breakpoint went with its debugger");
    assert!(lines::stopped(&next, ()).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_breakpoint_for_one_node_stops_only_it() {
    use client::witgraph::debug::lines::{self, BreakpointScope};

    let (_served, wrpc, file, line) = lines_attached().await;
    // The walker runs this line at every count: a breakpoint for another node never stops it.
    let other = BreakpointScope::Node("counter".into());
    lines::set_breakpoint(&wrpc, (), &other, &file, line)
        .await
        .unwrap();
    // Asked by `stopped`: a `next_stop` given up on would still wait where it was served.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let early = lines::stopped(&wrpc, ()).await.unwrap();
    assert!(early.is_none(), "stopped for another node: {early:?}");

    let walker = BreakpointScope::Node("walker".into());
    lines::set_breakpoint(&wrpc, (), &walker, &file, line)
        .await
        .unwrap();
    let stop = tokio::time::timeout(Duration::from_secs(20), lines::next_stop(&wrpc, ()))
        .await
        .expect("it stops")
        .unwrap();
    assert_eq!((stop.line, stop.node.as_deref()), (line, Some("walker")));
    lines::clear_breakpoint(&wrpc, (), &walker, &file, line)
        .await
        .unwrap();
    lines::resume(&wrpc, ()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debugger_stops_the_programs_graph_and_runs_it_again() {
    let (served, wrpc, _, _) = lines_attached().await;
    // What the counter has counted: it moves on at each of the program's ticks.
    let counted = || async {
        graph::read_output(&wrpc, (), "counter", "out")
            .await
            .unwrap()
            .unwrap()
    };
    assert!(graph::running(&wrpc, ()).await.unwrap());

    graph::stop(&wrpc, ()).await.unwrap();
    assert!(!graph::running(&wrpc, ()).await.unwrap());
    // As a graph run in the editor's page stops: what was under way is cancelled.
    let state = graph::node_state(&wrpc, (), "counter")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.phase, "cancelled");
    let stopped = counted().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(counted().await, stopped, "no tick advances it");

    graph::run(&wrpc, ()).await.unwrap();
    assert!(graph::running(&wrpc, ()).await.unwrap());
    tokio::time::timeout(Duration::from_secs(20), async {
        while counted().await == stopped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("its ticks advance it again");

    // A debugger gone while it is stopped does not leave it so.
    graph::stop(&wrpc, ()).await.unwrap();
    drop(wrpc);
    let next = connect(&served, "secret").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !graph::running(&next, ()).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the program runs it again");
}
