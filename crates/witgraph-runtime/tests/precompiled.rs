//! Saving prepared components and loading them back.
#![cfg(feature = "precompiled")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use witgraph_runtime::RunningGraph;

use test_components::{CALC, ECHO, Guest, MATH};
use witgraph_ir::{ComponentContract, ComponentRef, Graph};
use witgraph_runtime::{
    LinkedProvider, LoadError, NoCapabilities, Perf, PreparedComponent, RuntimeConfig, TickResult,
    Val,
};

type RuntimeGraph = witgraph_runtime::RuntimeGraph<Perf>;

fn contract(guest: Guest) -> ComponentContract {
    witgraph_wit::load_components(guest.wit).unwrap().remove(0)
}

/// Loads `bytes`, as another process would: on an engine of its own.
fn reload(contract: &ComponentContract, bytes: &[u8]) -> Result<PreparedComponent, LoadError> {
    let engine = RuntimeConfig::default().new_engine().unwrap();
    // SAFETY: the bytes come from `serialize`, or are refused unrun.
    unsafe { PreparedComponent::deserialize(&engine, contract, bytes) }
}

fn saved_echo() -> Vec<u8> {
    let engine = RuntimeConfig::default().new_engine().unwrap();
    PreparedComponent::new(&engine, &contract(ECHO), ECHO.wasm)
        .unwrap()
        .serialize()
        .unwrap()
}

async fn settle(rt: &mut RuntimeGraph) {
    while !matches!(rt.tick().await, TickResult::Idle) {}
}

#[tokio::test]
async fn a_saved_component_runs_on_a_new_engine() {
    let echo = contract(ECHO);
    let prepared = reload(&echo, &saved_echo()).unwrap();
    assert_eq!(prepared.contract(), &echo);

    let compiled = Graph::builder("saved")
        .add_component(&echo)
        .add_node("e", echo.id.clone())
        .build()
        .compile(std::slice::from_ref(&echo))
        .unwrap();
    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        &[prepared],
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
async fn a_saved_component_keeps_its_links() {
    let calc = contract(CALC);
    let math: ComponentRef = "test:math/math@0.1.0".parse().unwrap();
    const OPS: &str = "test:math/ops@0.1.0";
    let engine = RuntimeConfig::default().new_engine().unwrap();
    let links = [LinkedProvider {
        import: OPS,
        provider: &math,
        bytes: MATH.wasm,
        export: OPS,
    }];
    let saved = PreparedComponent::linked(&engine, &calc, CALC.wasm, &links)
        .unwrap()
        .serialize()
        .unwrap();

    let compiled = Graph::builder("saved")
        .add_component(&calc)
        .add_node("n", calc.id.clone())
        .link("l", "n", OPS, math.clone(), OPS)
        .build()
        .compile(std::slice::from_ref(&calc))
        .unwrap();
    let mut rt = RuntimeGraph::load_prepared(
        compiled,
        &[reload(&calc, &saved).unwrap()],
        RuntimeConfig::default(),
        Perf,
        NoCapabilities,
    )
    .await
    .expect("the link came back with the component");
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
fn bytes_saved_for_another_contract_are_refused() {
    let mut other = contract(ECHO);
    other.inputs.pop();
    let err = reload(&other, &saved_echo())
        .err()
        .expect("another contract");
    assert!(
        matches!(&err, LoadError::ContractMismatch { message, .. }
            if message.contains("saved for another contract")),
        "{err}"
    );
}

#[test]
fn anything_else_is_not_a_saved_component() {
    let echo = contract(ECHO);
    let saved = saved_echo();
    let empty: &[u8] = &[];
    let header_cut_short = &saved[..10];
    let raw_wasm = ECHO.wasm;
    for bytes in [empty, header_cut_short, raw_wasm] {
        let err = reload(&echo, bytes).err().expect("not a saved component");
        assert!(
            matches!(&err, LoadError::BadComponent { message, .. }
                if message.contains("not a saved component")),
            "{err}"
        );
    }
    let code_cut_short = &saved[..saved.len() / 2];
    let err = reload(&echo, code_cut_short)
        .err()
        .expect("its code is cut short");
    assert!(
        matches!(&err, LoadError::BadComponent { message, .. }
            if message.contains("failed to load its code")),
        "{err}"
    );
}

#[test]
fn an_engine_with_other_settings_refuses_the_code() {
    let echo = contract(ECHO);
    let config = RuntimeConfig {
        memory_reservation: Some(1 << 20),
        ..RuntimeConfig::default()
    };
    let engine = config.new_engine().unwrap();
    // SAFETY: the bytes come from `serialize`.
    let err = unsafe { PreparedComponent::deserialize(&engine, &echo, &saved_echo()) }
        .err()
        .expect("compiled for other settings");
    assert!(
        matches!(&err, LoadError::BadComponent { message, .. }
            if message.contains("failed to load its code")),
        "{err}"
    );
}

#[test]
fn an_engine_that_cannot_run_a_graph_is_refused() {
    let engine = wasmtime::Engine::default();
    // SAFETY: refused before the bytes are looked at.
    let err = unsafe { PreparedComponent::deserialize(&engine, &contract(ECHO), &[]) }
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
    wasm: &[u8],
    links: &[LinkedProvider<'_>],
    path: &Path,
) -> Result<PreparedComponent, LoadError> {
    let engine = config.new_engine().unwrap();
    let contract = contract(if links.is_empty() { ECHO } else { CALC });
    // SAFETY: the file comes from `serialize`, or is refused unrun.
    unsafe { PreparedComponent::load_or_compile(&engine, &contract, wasm, links, path) }
}

#[test]
fn load_or_compile_saves_once_and_then_loads() {
    let scratch = Scratch::new("once");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    load_or_compile(&config, ECHO.wasm, &[], &path).unwrap();
    let saved = std::fs::read(&path).unwrap();
    let at = modified(&path);

    let prepared = load_or_compile(&config, ECHO.wasm, &[], &path).unwrap();
    assert_eq!(modified(&path), at, "loaded, not compiled and saved again");
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    assert_eq!(prepared.contract(), &contract(ECHO));
    let leftovers = std::fs::read_dir(&scratch.0).unwrap().count();
    assert_eq!(leftovers, 1, "no temporary file stays behind");
}

#[test]
fn load_or_compile_accepts_a_file_saved_ahead_of_time() {
    let scratch = Scratch::new("ahead");
    let path = scratch.file();
    std::fs::write(&path, saved_echo()).unwrap();
    let at = modified(&path);
    load_or_compile(&RuntimeConfig::default(), ECHO.wasm, &[], &path).unwrap();
    assert_eq!(modified(&path), at);
}

#[test]
fn load_or_compile_replaces_a_stale_or_damaged_file() {
    let scratch = Scratch::new("stale");
    let path = scratch.file();
    let config = RuntimeConfig::default();

    std::fs::write(&path, b"damaged").unwrap();
    load_or_compile(&config, ECHO.wasm, &[], &path).unwrap();
    let good = std::fs::read(&path).unwrap();
    assert_ne!(good, b"damaged");
    reload(&contract(ECHO), &good).unwrap();

    // Other engine settings: the code in the file does not fit them.
    let other = RuntimeConfig {
        memory_reservation: Some(1 << 20),
        ..RuntimeConfig::default()
    };
    load_or_compile(&other, ECHO.wasm, &[], &path).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), good);
    let engine = other.new_engine().unwrap();
    let saved = std::fs::read(&path).unwrap();
    // SAFETY: just saved.
    unsafe { PreparedComponent::deserialize(&engine, &contract(ECHO), &saved) }.unwrap();
}

#[test]
fn load_or_compile_notices_a_rebuilt_component_with_the_same_contract() {
    let scratch = Scratch::new("rebuilt");
    let path = scratch.file();
    let config = RuntimeConfig::default();
    load_or_compile(&config, ECHO.wasm, &[], &path).unwrap();
    let old = std::fs::read(&path).unwrap();

    let new = rebuilt(ECHO.wasm);
    load_or_compile(&config, &new, &[], &path).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), old);

    let at = modified(&path);
    load_or_compile(&config, &new, &[], &path).unwrap();
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
    let math: ComponentRef = "test:math/math@0.1.0".parse().unwrap();
    const OPS: &str = "test:math/ops@0.1.0";
    let link = |bytes| LinkedProvider {
        import: OPS,
        provider: &math,
        bytes,
        export: OPS,
    };
    load_or_compile(&config, CALC.wasm, &[link(MATH.wasm)], &path).unwrap();
    let at = modified(&path);
    load_or_compile(&config, CALC.wasm, &[link(MATH.wasm)], &path).unwrap();
    assert_eq!(modified(&path), at);

    let new = rebuilt(MATH.wasm);
    load_or_compile(&config, CALC.wasm, &[link(&new)], &path).unwrap();
    assert_ne!(modified(&path), at);
}

#[test]
fn load_or_compile_reports_what_it_cannot_do() {
    let scratch = Scratch::new("errors");
    let path = scratch.file();
    let config = RuntimeConfig::default();

    let err = load_or_compile(&config, b"not wasm", &[], &path)
        .err()
        .expect("nothing to compile");
    assert!(matches!(err, LoadError::BadComponent { .. }), "{err}");
    assert!(!path.exists(), "nothing was saved");

    // A good file stays when what is given cannot replace it.
    load_or_compile(&config, ECHO.wasm, &[], &path).unwrap();
    let good = std::fs::read(&path).unwrap();
    load_or_compile(&config, b"not wasm", &[], &path)
        .err()
        .expect("nothing to compile");
    assert_eq!(std::fs::read(&path).unwrap(), good);

    let nowhere = scratch.0.join("missing").join("saved.bin");
    let err = load_or_compile(&config, ECHO.wasm, &[], &nowhere)
        .err()
        .expect("nowhere to save");
    assert!(
        matches!(&err, LoadError::BadComponent { message, .. }
            if message.contains("failed to save it")),
        "{err}"
    );

    std::fs::write(&path, b"damaged").unwrap();
    let engine = wasmtime::Engine::default();
    // SAFETY: refused before the file is run.
    let err = unsafe {
        PreparedComponent::load_or_compile(&engine, &contract(ECHO), ECHO.wasm, &[], &path)
    }
    .err()
    .expect("no fuel, no concurrency");
    assert!(matches!(err, LoadError::Runtime(_)), "{err}");
}
