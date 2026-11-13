//! How fast a Value moves between nodes on wasmtime, by size: a `list<u8>`
//! through source → relay → relay → sink, once per run.
//!
//! ```text
//! cargo run --release -p witgraph-runtime --example transfer [MiB...]
//! ```

use std::error::Error;
use std::time::{Duration, Instant};
use witgraph_runtime::RunningGraph;

use test_components::{BLOB_RELAY, BLOB_SINK, BLOB_SOURCE, Guest};
use witgraph_runtime::witgraph_ir::{Graph, PortRef};
use witgraph_runtime::{Perf, RuntimeConfig, RuntimeGraph, TickResult, Val};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let sizes = std::env::args()
        .skip(1)
        .map(|a| a.parse::<f64>())
        .collect::<Result<Vec<_>, _>>()?;
    let sizes = if sizes.is_empty() {
        vec![1.0, 4.0, 16.0]
    } else {
        sizes
    };

    let guests = [BLOB_SOURCE, BLOB_RELAY, BLOB_SINK];
    let kit = test_components::Kit::new(&guests);
    let contracts: Vec<_> = guests
        .iter()
        .map(|g: &Guest| kit.contract(&kit.get(*g)).clone())
        .collect();
    let wasm = kit.artifacts();
    let [source, relay, sink] = [0, 1, 2].map(|i| contracts[i].id.clone());
    let port = |node: &str| PortRef::new(node, "blob");
    let graph = contracts
        .iter()
        .fold(Graph::builder("transfer"), |b, c| b.add_component(c))
        .add_node("source", source)
        .add_node("a", relay.clone())
        .add_node("b", relay)
        .add_node("sink", sink)
        .connect("source-a", port("source"), port("a"))
        .connect("a-b", port("a"), port("b"))
        .connect("b-sink", port("b"), port("sink"))
        .build();

    println!("size      wasmtime");
    for mib in sizes {
        let size = (mib * 1024.0 * 1024.0).round() as u32;
        let compiled = graph
            .clone()
            .compile(&contracts)
            .map_err(|e| e.to_string())?;
        // Lifting a `list<u8>` costs about 48 bytes of hostcall fuel a
        // byte: the default (4 GiB) stops a list of ~85 MiB leaving a node.
        let config = RuntimeConfig {
            hostcall_fuel: usize::MAX / 2,
            ..RuntimeConfig::default()
        };
        let mut rt = RuntimeGraph::load(compiled, &wasm, config, Perf).await?;
        rt.inject(&"source".into(), &"size".into(), Val::U32(size))
            .await?;
        let runs = if mib >= 8.0 { 3 } else { 7 };
        let mut times = Vec::new();
        // The first run warms up.
        for seed in 0..=runs {
            rt.inject(&"source".into(), &"seed".into(), Val::U32(seed))
                .await?;
            let start = Instant::now();
            while !matches!(rt.tick().await, TickResult::Idle) {}
            let elapsed = start.elapsed();
            match rt.read_output(&"sink".into(), &"len".into()).await? {
                Some(Val::U32(len)) if len == size => {}
                other => {
                    let faults = rt.take_faults();
                    return Err(format!("the bytes did not arrive ({other:?}): {faults:?}").into());
                }
            }
            if seed > 0 {
                times.push(elapsed);
            }
        }
        times.sort();
        let median: Duration = times[times.len() / 2];
        println!("{mib} MiB   {:.1} ms", median.as_secs_f64() * 1000.0);
    }
    Ok(())
}
