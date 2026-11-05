//! A debugger attaching over WebTransport, as the editor does from a browser.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::time::Duration;

use test_components::ECHO;
use witgraph_ir::{ComponentRef, Graph, NodeId, PortName, PortRef};
use witgraph_remote::{Debuggee, Served, serve};
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
    let rt = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf)
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
