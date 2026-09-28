//! End-to-end proof: WIT fixtures are loaded and lowered, wired into a graph
//! exercising all four port kinds and a feedback boundary, compiled,
//! serialized editor-independently, and reflected into the committed metadata
//! catalog.

// Test helpers outside #[test] fns aren't covered by allow-*-in-tests.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;

use witgraph_wit::ir::{
    Capability, ComponentContract, ConsumptionMode, Diagnostic, Graph, PortKind, PortRef,
};
use witgraph_wit::{load_components, metadata};

fn fixtures() -> Vec<ComponentContract> {
    load_components(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/demo"))
        .expect("fixtures load and lower")
}

fn contract<'a>(contracts: &'a [ComponentContract], world: &str) -> &'a ComponentContract {
    contracts
        .iter()
        .find(|c| c.id.world == world)
        .unwrap_or_else(|| panic!("no `{world}` contract"))
}

/// sensor ─samples→ filter ─filtered→ display, filter ─done→ display,
/// sensor ─samples→ collector (drained input), sensor ─threshold-crossed→
/// alarm, accumulator state loop via feedback.
fn demo_graph(contracts: &[ComponentContract]) -> Graph {
    let mut builder = Graph::builder("demo");
    for c in contracts {
        builder = builder.add_component(c.clone());
    }
    for world in [
        "sensor",
        "filter",
        "display",
        "alarm",
        "accumulator",
        "collector",
    ] {
        builder = builder.add_node(world, contract(contracts, world).id.clone());
    }
    builder
        .connect(
            "samples",
            PortRef::new("sensor", "samples"),
            PortRef::new("filter", "raw"),
        )
        .connect(
            "collect",
            PortRef::new("sensor", "samples"),
            PortRef::new("collector", "samples"),
        )
        .connect(
            "view",
            PortRef::new("filter", "filtered"),
            PortRef::new("display", "view"),
        )
        .connect(
            "done",
            PortRef::new("filter", "done"),
            PortRef::new("display", "done"),
        )
        .connect(
            "threshold",
            PortRef::new("sensor", "threshold-crossed"),
            PortRef::new("alarm", "trigger"),
        )
        .connect_feedback(
            "state",
            PortRef::new("accumulator", "state-out"),
            PortRef::new("accumulator", "state-in"),
        )
        .build()
}

#[test]
fn fixtures_lower_with_all_four_port_kinds() {
    let contracts = fixtures();
    assert_eq!(contracts.len(), 6);

    let kind = |world: &str, port: &str| {
        let c = contract(&contracts, world);
        c.inputs
            .iter()
            .chain(&c.outputs)
            .find(|p| p.name.as_str() == port)
            .unwrap_or_else(|| panic!("no port `{world}.{port}`"))
            .kind
    };
    assert_eq!(kind("sensor", "latest"), PortKind::Value);
    assert_eq!(kind("sensor", "samples"), PortKind::Stream);
    assert_eq!(kind("sensor", "threshold-crossed"), PortKind::Event);
    assert_eq!(kind("filter", "done"), PortKind::Future);

    let collector = contract(&contracts, "collector");
    let samples = collector
        .inputs
        .iter()
        .find(|p| p.name.as_str() == "samples")
        .expect("collector has a `samples` input");
    assert!(samples.drained);
    assert_eq!(collector.consumption_mode(), ConsumptionMode::Sync);

    for c in &contracts {
        let hash = c.id.content_hash.as_deref().expect("every contract hashed");
        assert_eq!(hash.len(), 64);
    }
}

#[test]
fn demo_graph_compiles_and_aggregates_capabilities() {
    let contracts = fixtures();
    let compiled = demo_graph(&contracts)
        .compile()
        .expect("demo graph is valid");

    assert!(compiled.warnings().is_empty(), "{}", compiled.warnings());
    assert_eq!(
        compiled.required_capabilities(),
        BTreeSet::from([
            Capability::new("demo:caps/clock@0.1.0"),
            Capability::new("demo:caps/log@0.1.0"),
        ])
    );

    let order = compiled.topological_order();
    let position = |node: &str| {
        order
            .iter()
            .position(|id| id.as_str() == node)
            .unwrap_or_else(|| panic!("`{node}` missing from order"))
    };
    assert!(position("sensor") < position("filter"));
    assert!(position("filter") < position("display"));
    assert!(position("sensor") < position("alarm"));
}

#[test]
fn value_to_stream_connection_rejected() {
    let contracts = fixtures();
    let mut graph = demo_graph(&contracts);
    let samples = graph
        .connections
        .iter_mut()
        .find(|c| c.id.as_str() == "samples")
        .unwrap();
    samples.from = PortRef::new("sensor", "latest");

    let failure = graph.compile().unwrap_err();
    let diags = failure.diagnostics.into_vec();
    assert_eq!(diags.len(), 1);
    assert!(matches!(
        diags[0],
        Diagnostic::KindMismatch {
            from: PortKind::Value,
            to: PortKind::Stream,
            ..
        }
    ));
}

#[test]
fn second_writer_rejected() {
    let contracts = fixtures();
    let mut graph = demo_graph(&contracts);
    let mut second = graph
        .connections
        .iter()
        .find(|c| c.id.as_str() == "samples")
        .unwrap()
        .clone();
    second.id = "samples-again".into();
    second.from = PortRef::new("sensor", "samples");
    second.to = PortRef::new("display", "view");
    graph.connections.push(second);

    let failure = graph.compile().unwrap_err();
    let diags = failure.diagnostics.into_vec();
    assert_eq!(diags.len(), 1);
    match &diags[0] {
        Diagnostic::MultipleWriters { port, connections } => {
            assert_eq!(*port, PortRef::new("display", "view"));
            assert_eq!(connections.len(), 2);
        }
        other => panic!("expected MultipleWriters, got {other:?}"),
    }
}

#[test]
fn state_loop_without_feedback_flag_rejected() {
    let contracts = fixtures();
    let mut graph = demo_graph(&contracts);
    let state = graph
        .connections
        .iter_mut()
        .find(|c| c.id.as_str() == "state")
        .unwrap();
    state.feedback = false;

    let failure = graph.compile().unwrap_err();
    assert!(failure.diagnostics.iter().any(|d| matches!(
        d,
        Diagnostic::IllegalCycle { nodes } if nodes.iter().any(|n| n.as_str() == "accumulator")
    )));
}

#[test]
fn serialized_graph_is_editor_independent() {
    let contracts = fixtures();
    let graph = demo_graph(&contracts);

    let json = serde_json::to_string_pretty(&graph).unwrap();
    let restored: Graph = serde_json::from_str(&json).unwrap();
    assert_eq!(graph, restored);

    // The deserialized graph is self-contained: it re-compiles with no
    // resolver, filesystem, or editor state.
    restored
        .compile()
        .expect("round-tripped graph still compiles");
}

#[test]
fn catalog_matches_committed_golden() {
    let catalog = metadata::generate_catalog(&fixtures());
    let json = metadata::to_json(&catalog).unwrap();
    let golden_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/expected_catalog.json"
    );

    if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        std::fs::write(golden_path, &json).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(golden_path)
        .expect("golden catalog missing — regenerate with UPDATE_GOLDEN=1");
    assert_eq!(
        json, golden,
        "catalog drifted — regenerate with UPDATE_GOLDEN=1"
    );
}
