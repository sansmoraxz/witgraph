//! Saving prepared graphs and loading them back.
#![cfg(feature = "precompiled")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use witgraph_runtime::RunningGraph;

use test_components::{CALC, ECHO, Guest, MATH};
use witgraph_ir::{CompiledGraph, ComponentContract, ComponentRef, Graph};
use witgraph_runtime::{
    LoadError, NoCapabilities, Perf, PreparedGraph, RuntimeConfig, TickResult, Val,
};

type RuntimeGraph = witgraph_runtime::RuntimeGraph<Perf>;

const OPS: &str = "test:math/ops@0.1.0";

fn contract(guest: Guest) -> ComponentContract {
    witgraph_wit::load_components(guest.wit).unwrap().remove(0)
}

fn math() -> ComponentRef {
    "test:math/math@0.1.0".parse().unwrap()
}

fn echo_graph() -> CompiledGraph {
    let echo = contract(ECHO);
    Graph::builder("saved")
        .add_component(&echo)
        .add_node("e", echo.id.clone())
        .build()
        .compile(std::slice::from_ref(&echo))
        .unwrap()
}

fn calc_graph() -> CompiledGraph {
    let calc = contract(CALC);
    Graph::builder("saved")
        .add_component(&calc)
        .add_node("n", calc.id.clone())
        .link("l", "n", OPS, math(), OPS)
        .build()
        .compile(std::slice::from_ref(&calc))
        .unwrap()
}

fn echo_wasm(bytes: &[u8]) -> HashMap<ComponentRef, &[u8]> {
    [(contract(ECHO).id, bytes)].into_iter().collect()
}

fn calc_wasm(math_bytes: &[u8]) -> HashMap<ComponentRef, &[u8]> {
    [(contract(CALC).id, CALC.wasm), (math(), math_bytes)]
        .into_iter()
        .collect()
}

/// Loads `bytes`, as another process would: on an engine of its own.
fn reload(compiled: &CompiledGraph, bytes: &[u8]) -> Result<PreparedGraph, LoadError> {
    let engine = RuntimeConfig::default().new_engine().unwrap();
    // SAFETY: the bytes come from `serialize`, or are refused unrun.
    unsafe { PreparedGraph::deserialize(&engine, compiled, bytes) }
}

fn saved_echo() -> Vec<u8> {
    let engine = RuntimeConfig::default().new_engine().unwrap();
    PreparedGraph::new(&engine, &echo_graph(), &echo_wasm(ECHO.wasm))
        .unwrap()
        .serialize()
        .unwrap()
}

async fn settle(rt: &mut RuntimeGraph) {
    while !matches!(rt.tick().await, TickResult::Idle) {}
}

#[tokio::test]
async fn a_saved_graph_runs_on_a_new_engine() {
    let compiled = echo_graph();
    let prepared = reload(&compiled, &saved_echo()).unwrap();
    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        &prepared,
        RuntimeConfig::default(),
        Perf,
        NoCapabilities,
    )
    .await
    .unwrap();
    rt.inject(&"e".into(), &"in".into(), Val::Float64(1.5))
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.read_output(&"e".into(), &"out".into()).await.unwrap(),
        Some(Val::Float64(1.5))
    );
}

#[tokio::test]
async fn a_saved_graph_keeps_its_links() {
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let saved = PreparedGraph::new(&engine, &calc_graph(), &calc_wasm(MATH.wasm))
        .unwrap()
        .serialize()
        .unwrap();
    let compiled = calc_graph();
    let prepared = reload(&compiled, &saved).unwrap();
    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        &prepared,
        RuntimeConfig::default(),
        Perf,
        NoCapabilities,
    )
    .await
    .expect("the link came back with the graph");
    rt.inject(&"n".into(), &"x".into(), Val::U32(2))
        .await
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.read_output(&"n".into(), &"doubled".into())
            .await
            .unwrap(),
        Some(Val::U32(4))
    );
}

#[test]
fn bytes_saved_for_another_graph_are_refused() {
    let echo = contract(ECHO);
    let other = Graph::builder("saved")
        .add_component(&echo)
        .add_node("renamed", echo.id.clone())
        .build()
        .compile(std::slice::from_ref(&echo))
        .unwrap();
    let err = reload(&other, &saved_echo()).err().expect("another graph");
    assert!(
        matches!(&err, LoadError::GraphMismatch { message }
            if message.contains("saved for another graph")),
        "{err}"
    );
}

#[test]
fn anything_else_is_not_a_saved_graph() {
    let compiled = echo_graph();
    let saved = saved_echo();
    let empty: &[u8] = &[];
    let header_cut_short = &saved[..10];
    let raw_wasm = ECHO.wasm;
    for bytes in [empty, header_cut_short, raw_wasm] {
        let err = reload(&compiled, bytes).err().expect("not a saved graph");
        assert!(
            matches!(&err, LoadError::GraphMismatch { message }
                if message.contains("not a saved graph")),
            "{err}"
        );
    }
    let code_cut_short = &saved[..saved.len() / 2];
    let err = reload(&compiled, code_cut_short)
        .err()
        .expect("its code is cut short");
    assert!(
        matches!(&err, LoadError::GraphMismatch { message }
            if message.contains("failed to load its code")),
        "{err}"
    );
}

#[test]
fn an_engine_with_other_settings_refuses_the_code() {
    let config = RuntimeConfig {
        memory_reservation: Some(witgraph_runtime::ByteCount(1 << 20)),
        ..RuntimeConfig::default()
    };
    let engine = config.new_engine().unwrap();
    // SAFETY: the bytes come from `serialize`.
    let err = unsafe { PreparedGraph::deserialize(&engine, &echo_graph(), &saved_echo()) }
        .err()
        .expect("compiled for other settings");
    assert!(
        matches!(&err, LoadError::GraphMismatch { message }
            if message.contains("failed to load its code")),
        "{err}"
    );
}

#[test]
fn an_engine_that_cannot_run_a_graph_is_refused() {
    let engine = wasmtime::Engine::default();
    // SAFETY: refused before the bytes are looked at.
    let err = unsafe { PreparedGraph::deserialize(&engine, &echo_graph(), &[]) }
        .err()
        .expect("no fuel, no concurrency");
    assert!(matches!(err, LoadError::Runtime(_)), "{err}");
}

/// A directory of its own for one test, removed after it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "witgraph-precompiled-{}-{test}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("saved.bin")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// `wasm` with a custom section added: other bytes, the same contract.
fn rebuilt(wasm: &[u8]) -> Vec<u8> {
    [wasm, &[0, 2, 1, b'x']].concat()
}

fn load_or_compile(
    config: &RuntimeConfig,
    compiled: &CompiledGraph,
    wasm: &HashMap<ComponentRef, &[u8]>,
    path: &Path,
) -> Result<PreparedGraph, LoadError> {
    let engine = config.new_engine().unwrap();
    // SAFETY: the file comes from `serialize`, or is refused unrun.
    unsafe { PreparedGraph::load_or_compile(&engine, compiled, wasm, path) }
}

#[test]
fn load_or_compile_saves_once_and_then_loads() {
    let scratch = Scratch::new("once");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    let (graph, wasm) = (echo_graph(), echo_wasm(ECHO.wasm));
    load_or_compile(&config, &graph, &wasm, &path).unwrap();
    let saved = std::fs::read(&path).unwrap();
    let at = modified(&path);

    load_or_compile(&config, &graph, &wasm, &path).unwrap();
    assert_eq!(modified(&path), at, "loaded, not compiled and saved again");
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    let leftovers = std::fs::read_dir(&scratch.0).unwrap().count();
    assert_eq!(leftovers, 1, "no temporary file stays behind");
}

#[test]
fn load_or_compile_accepts_a_file_saved_ahead_of_time() {
    let scratch = Scratch::new("ahead");
    let path = scratch.file();
    std::fs::write(&path, saved_echo()).unwrap();
    let at = modified(&path);
    load_or_compile(
        &RuntimeConfig::default(),
        &echo_graph(),
        &echo_wasm(ECHO.wasm),
        &path,
    )
    .unwrap();
    assert_eq!(modified(&path), at);
}

#[test]
fn load_or_compile_replaces_a_stale_or_damaged_file() {
    let scratch = Scratch::new("stale");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    let (graph, wasm) = (echo_graph(), echo_wasm(ECHO.wasm));

    std::fs::write(&path, b"damaged").unwrap();
    load_or_compile(&config, &graph, &wasm, &path).unwrap();
    let good = std::fs::read(&path).unwrap();
    assert_ne!(good, b"damaged");
    reload(&graph, &good).unwrap();

    // Other engine settings: the code in the file does not fit them.
    let other = RuntimeConfig {
        memory_reservation: Some(witgraph_runtime::ByteCount(1 << 20)),
        ..RuntimeConfig::default()
    };
    load_or_compile(&other, &graph, &wasm, &path).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), good);
    let engine = other.new_engine().unwrap();
    let saved = std::fs::read(&path).unwrap();
    // SAFETY: just saved.
    unsafe { PreparedGraph::deserialize(&engine, &graph, &saved) }.unwrap();
}

#[test]
fn load_or_compile_notices_a_rebuilt_component_with_the_same_contract() {
    let scratch = Scratch::new("rebuilt");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    let graph = echo_graph();
    load_or_compile(&config, &graph, &echo_wasm(ECHO.wasm), &path).unwrap();
    let old = std::fs::read(&path).unwrap();

    let new = rebuilt(ECHO.wasm);
    load_or_compile(&config, &graph, &echo_wasm(&new), &path).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), old);

    let at = modified(&path);
    load_or_compile(&config, &graph, &echo_wasm(&new), &path).unwrap();
    assert_eq!(
        modified(&path),
        at,
        "the new file is good for the new bytes"
    );
}

#[test]
fn load_or_compile_notices_a_changed_link() {
    let scratch = Scratch::new("link");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    let graph = calc_graph();
    load_or_compile(&config, &graph, &calc_wasm(MATH.wasm), &path).unwrap();
    let at = modified(&path);
    load_or_compile(&config, &graph, &calc_wasm(MATH.wasm), &path).unwrap();
    assert_eq!(modified(&path), at);

    let new = rebuilt(MATH.wasm);
    load_or_compile(&config, &graph, &calc_wasm(&new), &path).unwrap();
    assert_ne!(modified(&path), at);
}

#[test]
fn load_or_compile_reports_what_it_cannot_do() {
    let scratch = Scratch::new("errors");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    let graph = echo_graph();

    let err = load_or_compile(&config, &graph, &echo_wasm(b"not wasm"), &path)
        .err()
        .expect("nothing to compile");
    assert!(matches!(err, LoadError::BadComponent { .. }), "{err}");
    assert!(!path.exists(), "nothing was saved");

    // A good file stays when what is given cannot replace it.
    load_or_compile(&config, &graph, &echo_wasm(ECHO.wasm), &path).unwrap();
    let good = std::fs::read(&path).unwrap();
    load_or_compile(&config, &graph, &echo_wasm(b"not wasm"), &path)
        .err()
        .expect("nothing to compile");
    assert_eq!(std::fs::read(&path).unwrap(), good);

    let nowhere = scratch.0.join("missing").join("saved.bin");
    let err = load_or_compile(&config, &graph, &echo_wasm(ECHO.wasm), &nowhere)
        .err()
        .expect("nowhere to save");
    assert!(
        matches!(&err, LoadError::GraphMismatch { message }
            if message.contains("failed to save it")),
        "{err}"
    );

    std::fs::write(&path, b"damaged").unwrap();
    let engine = wasmtime::Engine::default();
    let wasm = echo_wasm(ECHO.wasm);
    // SAFETY: refused before the file is run.
    let err = unsafe { PreparedGraph::load_or_compile(&engine, &graph, &wasm, &path) }
        .err()
        .expect("no fuel, no concurrency");
    assert!(matches!(err, LoadError::Runtime(_)), "{err}");
}
