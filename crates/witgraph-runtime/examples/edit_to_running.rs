//! How long an edit of a graph takes to reach a running one: generating the hub,
//! composing it with the nodes and compiling the result, by the number of nodes in a
//! chain, and instantiating it.
//!
//! ```text
//! cargo run --release -p witgraph-runtime --example edit_to_running [nodes...]
//! ```

use std::error::Error;
use std::time::Instant;

use test_components::{BLOB_RELAY, BLOB_SINK, BLOB_SOURCE};
use witgraph_ir::{CompiledGraph, ComponentContract, Graph, PortRef};
use witgraph_runtime::{NoCapabilities, Perf, PreparedGraph, RuntimeConfig, RuntimeGraph};

fn chain(nodes: usize, contracts: &[ComponentContract]) -> Result<CompiledGraph, Box<dyn Error>> {
    let name = |i: usize| format!("n{i}");
    let mut builder = contracts
        .iter()
        .fold(Graph::builder("chain"), |b, c| b.add_component(c));
    builder = builder.add_node(name(0), contracts[0].id.clone());
    for i in 1..nodes - 1 {
        builder = builder.add_node(name(i), contracts[1].id.clone());
    }
    builder = builder.add_node(name(nodes - 1), contracts[2].id.clone());
    for i in 1..nodes {
        builder = builder.connect(
            format!("c{i}"),
            PortRef::new(name(i - 1), "blob"),
            PortRef::new(name(i), "blob"),
        );
    }
    Ok(builder
        .build()
        .compile(contracts)
        .map_err(|e| e.to_string())?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let sizes: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| a.parse())
        .collect::<Result<_, _>>()?;
    let sizes = if sizes.is_empty() {
        vec![4, 16, 32]
    } else {
        sizes
    };
    let guests = [BLOB_SOURCE, BLOB_RELAY, BLOB_SINK];
    let kit = test_components::Kit::new(&guests);
    let contracts: Vec<ComponentContract> = guests
        .iter()
        .map(|g| kit.contract(&kit.get(*g)).clone())
        .collect();
    let wasm = kit.artifacts();
    println!("nodes  prepare    instantiate");
    for nodes in sizes {
        let engine = RuntimeConfig::default().new_engine()?;
        let start = Instant::now();
        let prepared = PreparedGraph::new(&engine, &chain(nodes, &contracts)?, &wasm)?;
        let prepare = start.elapsed();
        let start = Instant::now();
        let _rt: RuntimeGraph<Perf, NoCapabilities> = RuntimeGraph::load_prepared(
            chain(nodes, &contracts)?,
            &prepared,
            RuntimeConfig::default(),
            Perf,
            NoCapabilities,
        )
        .await?;
        println!("{nodes:<6} {:<10.1?} {:.1?}", prepare, start.elapsed());
    }
    Ok(())
}
