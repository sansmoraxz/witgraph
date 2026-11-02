//! Saving prepared components and loading them back.
#![cfg(feature = "precompiled")]
#![allow(clippy::unwrap_used)]

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
        .unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.read_output(&"e".into(), &"out".into()).unwrap(),
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
    rt.inject(&"n".into(), &"x".into(), Val::U32(2)).unwrap();
    settle(&mut rt).await;
    assert_eq!(
        rt.read_output(&"n".into(), &"doubled".into()).unwrap(),
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
