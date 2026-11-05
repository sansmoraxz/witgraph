//! The fuzz targets' checks on stable: seeded inputs, and every input that once failed.
//! `WITGRAPH_FUZZ_CASES` runs more.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use witgraph_fuzz::{REACHED, graph_json, graph_model, graph_run};

fn cases() -> u64 {
    std::env::var("WITGRAPH_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(3000)
}

fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

const RULES: [&str; 18] = [
    "duplicate_node_id",
    "duplicate_connection_id",
    "duplicate_connection",
    "unknown_node",
    "unknown_port",
    "not_an_output",
    "not_an_input",
    "kind_mismatch",
    "type_mismatch",
    "multiple_writers",
    "required_input_unconnected",
    "feedback_only_input",
    "async_fan_out",
    "async_feedback",
    "illegal_cycle",
    "useless_feedback",
    "group_overclaims",
    "group_waits_on_itself",
];

#[test]
fn seeded_graphs_follow_the_rules() {
    for seed in 0..cases() {
        let len = usize::try_from(seed % 400).unwrap() + 16;
        graph_model(&bytes(seed + 1, len));
    }
    let reached = |stage: &std::sync::atomic::AtomicUsize| stage.load(Ordering::Relaxed);
    let (parsed, compiled, global, grouped) = (
        reached(&REACHED.parsed),
        reached(&REACHED.compiled),
        reached(&REACHED.global),
        reached(&REACHED.grouped),
    );
    eprintln!("parsed {parsed}, compiled {compiled}, whole-graph {global}, grouped {grouped}");
    assert!(
        compiled * 20 > parsed,
        "too few compile: {compiled} of {parsed}"
    );
    assert!(
        global * 10 > parsed,
        "too few reach the whole-graph rules: {global} of {parsed}"
    );
    assert!(grouped > 0, "none compiled with a stream group");

    // Every rule must have been reached, or its agreement means nothing.
    let codes = REACHED.codes.lock().unwrap();
    eprintln!("{codes:#?}");
    for rule in RULES {
        let code = format!("witgraph::ir::{rule}");
        assert!(
            codes.contains_key(&code),
            "`{rule}` was never reached: {codes:#?}"
        );
    }
}

#[test]
fn seeded_graphs_run_as_they_should() {
    for seed in 0..cases() {
        let len = usize::try_from(seed % 400).unwrap() + 16;
        graph_run(&bytes(seed + 1, len));
    }
}

#[test]
fn inputs_that_once_failed() {
    type Target = fn(&[u8]);
    let targets: [(&str, Target); 3] = [
        ("graph_model", graph_model),
        ("graph_json", graph_json),
        ("graph_run", graph_run),
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("regressions");
    for (name, target) in targets {
        let Ok(entries) = fs::read_dir(root.join(name)) else {
            continue;
        };
        for entry in entries {
            target(&fs::read(entry.unwrap().path()).unwrap());
        }
    }
}
