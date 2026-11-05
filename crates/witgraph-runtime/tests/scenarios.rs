//! Runs `test-fixtures/scenarios` on wasmtime. `witgraph-loader`'s
//! `scenarios.test.ts` runs the same files on JavaScript, so the two hosts answer
//! to one set of expectations.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::{Value, json};
use test_components::{
    BUSY_LOOP, ECHO, FUTURE_READER, FUTURE_WRITER, Guest, MAYBE, RELAY, STREAM_CONSUMER,
    STREAM_PRODUCER, STREAM_RELAY,
};
use witgraph_ir::{ComponentContract, Graph, NodeId, PortName, PortRef, ResourceClaim};
use witgraph_runtime::{
    NodeFault, RunningGraph, RuntimeConfig, RuntimeGraph, Snapshot, TickResult, Trace, TraceEvent,
};

fn guest(name: &str) -> Guest {
    match name {
        "busy-loop" => BUSY_LOOP,
        "echo" => ECHO,
        "future-reader" => FUTURE_READER,
        "future-writer" => FUTURE_WRITER,
        "maybe" => MAYBE,
        "relay" => RELAY,
        "stream-consumer" => STREAM_CONSUMER,
        "stream-producer" => STREAM_PRODUCER,
        "stream-relay" => STREAM_RELAY,
        other => panic!("add `{other}` to the guests of this test"),
    }
}

fn contract(guest: Guest) -> ComponentContract {
    witgraph_wit::load_components(guest.wit).unwrap().remove(0)
}

/// `node.port`.
fn at(step: &Value) -> (NodeId, PortName) {
    let text = step["at"].as_str().expect("`at`");
    let (node, port) = text.split_once('.').expect("`node.port`");
    (node.into(), port.into())
}

fn port_ref(text: &Value) -> PortRef {
    let (node, port) = text.as_str().unwrap().split_once('.').unwrap();
    PortRef::new(node, port)
}

async fn load(scenario: &Value) -> RuntimeGraph<Trace> {
    let mut contracts: Vec<ComponentContract> = Vec::new();
    let mut wasm = HashMap::new();
    let mut builder = Graph::builder("scenario");
    for node in scenario["nodes"].as_array().unwrap() {
        let guest = guest(node["guest"].as_str().unwrap());
        let contract = contract(guest);
        if !contracts.iter().any(|c| c.id == contract.id) {
            builder = builder.add_component(&contract);
            wasm.insert(contract.id.clone(), guest.wasm);
            contracts.push(contract.clone());
        }
        let id = node["id"].as_str().unwrap();
        builder = builder.add_node(id, contract.id);
        if let Some(claims) = node["resources"].as_object() {
            for (resource, fraction) in claims {
                let claim = ResourceClaim::new(fraction.as_f64().unwrap()).unwrap();
                builder = builder.set_resource(id, resource.as_str(), claim).unwrap();
            }
        }
    }
    for conn in scenario["connections"].as_array().unwrap() {
        let (id, from, to) = (
            conn["id"].as_str().unwrap(),
            port_ref(&conn["from"]),
            port_ref(&conn["to"]),
        );
        builder = if conn["feedback"] == json!(true) {
            builder.connect_feedback(id, from, to)
        } else {
            builder.connect(id, from, to)
        };
    }
    let compiled = builder.build().compile(&contracts).expect("compiles");
    RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Trace::new())
        .await
        .expect("loads")
}

fn tick_name(result: &TickResult) -> &'static str {
    match result {
        TickResult::Progress => "progress",
        TickResult::Idle => "idle",
        TickResult::Interrupted => "interrupted",
        TickResult::StepLimitReached => "step-limit",
        TickResult::Faulted(_) => "faulted",
    }
}

fn fault_name(fault: &NodeFault) -> &'static str {
    match fault {
        NodeFault::WasmTrap { .. } => "trap",
        NodeFault::MemoryLimit { .. } => "memory-limit",
        NodeFault::HostcallFuelExhausted => "hostcall-fuel",
        NodeFault::FuelExhausted => "fuel",
        NodeFault::Fatal { .. } => "fatal",
        NodeFault::Restart { .. } => "restart",
    }
}

/// `result` against what `step` says of it: an error holding its `error`, or none.
fn check_outcome<E: std::fmt::Display>(step: &Value, result: Result<(), E>) {
    match (result, step["error"].as_str()) {
        (Ok(()), None) => {}
        (Ok(()), Some(wanted)) => panic!("{step}: no error, wanted one with `{wanted}`"),
        (Err(e), None) => panic!("{step}: {e}"),
        (Err(e), Some(wanted)) => {
            let message = e.to_string();
            assert!(message.contains(wanted), "{step}: {message}");
        }
    }
}

/// Runs `scenario`, panicking at the first step that does not go as it says.
async fn run(scenario: &Value) {
    let mut rt = load(scenario).await;
    let mut taken: Option<Value> = None;
    for step in scenario["steps"].as_array().unwrap() {
        match step["do"].as_str().unwrap() {
            "inject" => {
                let (node, port) = at(step);
                let wave = step["wave"].as_str().unwrap();
                check_outcome(step, rt.inject_wave(&node, &port, wave).await);
            }
            "clear" => {
                let (node, port) = at(step);
                check_outcome(step, rt.clear_input(&node, &port).await);
            }
            "rerun" => {
                let node = step["node"].as_str().unwrap().into();
                check_outcome(step, rt.rerun(&node));
            }
            "shutdown" => rt.shutdown(),
            "tick" => {
                let result = rt.tick().await;
                if let Some(wanted) = step["expect"].as_str() {
                    assert_eq!(tick_name(&result), wanted, "{step}");
                }
            }
            "settle" => {
                let mut settled = false;
                for _ in 0..100 {
                    match rt.tick().await {
                        TickResult::Idle => {
                            settled = true;
                            break;
                        }
                        TickResult::Progress => {}
                        other => panic!("{step}: {other:?}"),
                    }
                }
                assert!(settled, "{step}: never settled");
            }
            "output" => {
                let (node, port) = at(step);
                let read = rt.read_output_wave(&node, &port).await.unwrap();
                assert_eq!(json!(read), step["expect"], "{step}");
            }
            "runs" => {
                let mut runs: BTreeMap<String, u64> = BTreeMap::new();
                for event in rt.mode().take_trace() {
                    if let TraceEvent::RunStarted { node } = event {
                        *runs.entry(node.to_string()).or_default() += 1;
                    }
                }
                let wanted = step["expect"].as_object().unwrap();
                for (node, count) in wanted {
                    let ran = runs.get(node).copied().unwrap_or(0);
                    assert_eq!(json!(ran), *count, "{step}: runs of `{node}`");
                }
                let unlisted: Vec<_> = runs.keys().filter(|n| !wanted.contains_key(*n)).collect();
                assert!(unlisted.is_empty(), "{step}: ran {unlisted:?} too");
            }
            "faults" => {
                let faults = rt.take_faults();
                let wanted = step["expect"].as_array().unwrap();
                assert_eq!(faults.len(), wanted.len(), "{step}: {faults:?}");
                for (fault, wanted) in faults.iter().zip(wanted) {
                    let kind = wanted["kind"].as_str().or(wanted.as_str()).unwrap();
                    assert_eq!(fault_name(&fault.fault), kind, "{step}: {fault:?}");
                    if let Some(culprit) = wanted["culprit"].as_str() {
                        let found = fault.culprit.as_ref().map(|node| node.as_str());
                        assert_eq!(found, Some(culprit), "{step}");
                    }
                }
            }
            "phase" => {
                let node = step["node"].as_str().unwrap().into();
                let phase = serde_json::to_value(rt.node_state(&node).unwrap().phase()).unwrap();
                assert_eq!(phase, step["expect"], "{step}");
            }
            "snapshot" => {
                let snapshot = serde_json::to_value(rt.snapshot().await.unwrap()).unwrap();
                for (field, wanted) in step["expect"].as_object().unwrap() {
                    assert_eq!(&snapshot[field], wanted, "{step}: `{field}`: {snapshot}");
                }
                taken = Some(snapshot);
            }
            "restore" => {
                let mut snapshot = step
                    .get("snapshot")
                    .cloned()
                    .or_else(|| taken.clone())
                    .expect("a snapshot taken, or given");
                if let Some(patch) = step["patch"].as_object() {
                    for (field, value) in patch {
                        snapshot[field] = value.clone();
                    }
                }
                let snapshot: Snapshot = serde_json::from_value(snapshot).unwrap();
                check_outcome(step, rt.restore(&snapshot).await);
            }
            other => panic!("no step `{other}`"),
        }
    }
}

#[tokio::test]
async fn every_scenario_goes_as_it_says() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/scenarios");
    // Generated ones (see `fuzz/README.md`), and any more a run is pointed at.
    let extra = std::env::var_os("WITGRAPH_SCENARIOS").map(PathBuf::from);
    let mut files: Vec<PathBuf> = [Some(dir.clone()), Some(dir.join("generated")), extra]
        .into_iter()
        .flatten()
        .flat_map(|dir| std::fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios in {}", dir.display());
    for file in files {
        let scenario: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        eprintln!("scenario {name}");
        run(&scenario).await;
    }
}
