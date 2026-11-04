//! Compiling a component once and running it, later, in a build with no
//! compiler in it.
//!
//! ```text
//! cargo run --release -p witgraph-runtime --example precompiled --features precompiled -- save echo.bin
//! cargo run --release -p witgraph-runtime --example precompiled --no-default-features --features precompiled -- run echo.bin
//! ```

use std::error::Error;
use std::time::Instant;
use witgraph_runtime::RunningGraph;

use test_components::ECHO;
use witgraph_runtime::witgraph_ir::{CompiledGraph, ComponentContract, Graph};
use witgraph_runtime::{
    NoCapabilities, Perf, PreparedGraph, RuntimeConfig, RuntimeGraph, TickResult, Val,
};

fn graph(contract: &ComponentContract) -> Result<CompiledGraph, Box<dyn Error>> {
    Graph::builder("precompiled")
        .add_component(contract)
        .add_node("echo", contract.id.clone())
        .build()
        .compile(std::slice::from_ref(contract))
        .map_err(|e| e.to_string().into())
}

#[cfg(feature = "compile")]
fn save(contract: &ComponentContract, path: &str) -> Result<(), Box<dyn Error>> {
    let engine = RuntimeConfig::default().new_engine()?;
    let start = Instant::now();
    let wasm = [(contract.id.clone(), ECHO.wasm)].into_iter().collect();
    let prepared = PreparedGraph::new(&engine, &graph(contract)?, &wasm)?;
    println!("compiled in {:?}", start.elapsed());
    std::fs::write(path, prepared.serialize()?)?;
    Ok(())
}

#[cfg(not(feature = "compile"))]
fn save(_: &ComponentContract, _: &str) -> Result<(), Box<dyn Error>> {
    Err("this build has no compiler: enable the `compile` feature".into())
}

async fn run(contract: &ComponentContract, path: &str) -> Result<(), Box<dyn Error>> {
    let engine = RuntimeConfig::default().new_engine()?;
    let bytes = std::fs::read(path)?;
    let start = Instant::now();
    // SAFETY: this example wrote the file itself, with `save`.
    let compiled = graph(contract)?;
    let prepared = unsafe { PreparedGraph::deserialize(&engine, &compiled, &bytes) }?;
    println!("loaded in {:?}", start.elapsed());

    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        &prepared,
        RuntimeConfig::default(),
        Perf,
        NoCapabilities,
    )
    .await?;
    rt.inject(&"echo".into(), &"in".into(), Val::Float64(1.5))
        .await?;
    while !matches!(rt.tick().await, TickResult::Idle) {}
    let out = rt.read_output(&"echo".into(), &"out".into()).await?;
    println!("echo says {out:?}");
    if out != Some(Val::Float64(1.5)) {
        return Err("the value did not come back".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let contract = witgraph_wit::load_components(ECHO.wit)?.remove(0);
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["save", path] => save(&contract, path),
        ["run", path] => run(&contract, path).await,
        _ => Err("usage: precompiled (save | run) <path>".into()),
    }
}
