//! A graph on wasmtime that a debugger can attach to: a counter feeding a guest written in Rust
//! and built with debug info, which feeds one written in C, which feeds one written in Zig, each
//! built so too; ticked a few times a second. Prints where to attach, as one line of JSON.
//!
//! `cargo run -p witgraph-remote --example serve [address]`, then open the editor demo with
//! `?attach=<url>&hash=<certificateHash>`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::Write as _;
use std::time::Duration;

use test_components::{C_WALK, LINE_WALK, RELAY, ZIG_WALK};
use witgraph_ir::{Graph, NodeId, PortName, PortRef};
use witgraph_remote::{Debuggee, Watched, serve};
use witgraph_runtime::{OwnLines, Perf, RunningGraph, RuntimeConfig, RuntimeGraph};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:0".into());
    let mut contracts = Vec::new();
    let mut wasm = HashMap::new();
    for guest in [RELAY, LINE_WALK, C_WALK, ZIG_WALK] {
        let contract = witgraph_wit::load_components(guest.wit)?.remove(0);
        wasm.insert(contract.id.clone(), guest.wasm);
        contracts.push(contract);
    }
    let graph = Graph::builder("attached")
        .add_component(&contracts[0])
        .add_component(&contracts[1])
        .add_component(&contracts[2])
        .add_component(&contracts[3])
        .add_node("counter", contracts[0].id.clone())
        .add_node("walker", contracts[1].id.clone())
        .add_node("c-walker", contracts[2].id.clone())
        .add_node("zig-walker", contracts[3].id.clone())
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
        .connect(
            "walk-in-c",
            PortRef::new("walker", "out"),
            PortRef::new("c-walker", "in"),
        )
        .connect(
            "walk-in-zig",
            PortRef::new("c-walker", "out"),
            PortRef::new("zig-walker", "in"),
        )
        .build();
    let config = RuntimeConfig {
        debug_lines: Some(OwnLines::default()),
        ..RuntimeConfig::default()
    };
    let mut rt = RuntimeGraph::load(
        graph.compile(&contracts)?,
        &wasm,
        config,
        Watched::new(Perf),
    )
    .await?;
    rt.inject_wave(&NodeId::from("counter"), &PortName::from("in"), "0")
        .await?;
    rt.inject_wave(&NodeId::from("counter"), &PortName::from("add"), "1")
        .await?;
    // Each walker's second file, `scale`, runs only with this.
    for walker in ["walker", "c-walker", "zig-walker"] {
        rt.inject_wave(&NodeId::from(walker), &PortName::from("scale"), "2")
            .await?;
    }
    let debuggee = Debuggee::new(rt)?;
    // Unguessable enough for a demo on one machine.
    let token = format!(
        "{:016x}{:016x}",
        RandomState::new().hash_one(1),
        RandomState::new().hash_one(2)
    );
    let served = serve(debuggee.clone(), addr.parse()?, token).await?;
    let hash: String = served
        .certificate_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "{{\"url\":\"{}\",\"certificateHash\":\"{hash}\"}}",
        served.url
    );
    std::io::stdout().flush()?;
    loop {
        let _ = debuggee.tick().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
