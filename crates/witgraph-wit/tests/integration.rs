//! End-to-end proof: WIT fixtures are loaded and lowered, wired into a graph
//! exercising every port kind, stream islands and a feedback boundary,
//! compiled against the re-derived contracts, serialized as plain component
//! references, and reflected into the committed metadata catalog.

// Test helpers outside #[test] fns aren't covered by allow-*-in-tests.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;

use witgraph_wit::ir::{
    ComponentContract, Diagnostic, Graph, NodeShape, PortKind, PortRef, RunKind,
};
use witgraph_wit::{load_components, load_lowered, metadata};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/demo");

fn fixtures() -> Vec<ComponentContract> {
    load_components(FIXTURES).expect("fixtures load and lower")
}

fn contract<'a>(contracts: &'a [ComponentContract], world: &str) -> &'a ComponentContract {
    contracts
        .iter()
        .find(|c| c.id.world == world)
        .unwrap_or_else(|| panic!("no `{world}` contract"))
}

/// Island 1: sensor ─samples→ filter ─filtered→ display, filter ─done→
/// display, sensor ─threshold-crossed→ alarm.
/// Island 2: archive (a second sensor) ─samples→ collector.
/// Island 3: accumulator, whose state loops back over a feedback edge.
fn demo_graph(contracts: &[ComponentContract]) -> Graph {
    let mut builder = Graph::builder("demo");
    for c in contracts {
        builder = builder.add_component(c);
    }
    for (node, world) in [
        ("sensor", "sensor"),
        ("archive", "sensor"),
        ("filter", "filter"),
        ("display", "display"),
        ("alarm", "alarm"),
        ("collector", "collector"),
        ("accumulator", "accumulator"),
    ] {
        builder = builder.add_node(node, contract(contracts, world).id.clone());
    }
    builder
        .connect(
            "samples",
            PortRef::new("sensor", "samples"),
            PortRef::new("filter", "raw"),
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
        .connect(
            "collect",
            PortRef::new("archive", "samples"),
            PortRef::new("collector", "samples"),
        )
        .connect_feedback(
            "state",
            PortRef::new("accumulator", "state-out"),
            PortRef::new("accumulator", "state-in"),
        )
        .build()
}

fn diagnostics(graph: Graph, contracts: &[ComponentContract]) -> Vec<Diagnostic> {
    graph
        .compile(contracts)
        .expect_err("expected compilation failure")
        .diagnostics
        .into_vec()
}

#[test]
fn fixtures_lower_with_every_port_kind() {
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
    assert_eq!(kind("sensor", "threshold-crossed"), PortKind::Stream);
    assert_eq!(kind("filter", "done"), PortKind::Future);
    assert_eq!(kind("collector", "samples"), PortKind::Stream);

    let accumulator = contract(&contracts, "accumulator");
    assert_eq!(accumulator.run, RunKind::Sync);
    assert_eq!(accumulator.shape(), NodeShape::Reactive);
    let collector = contract(&contracts, "collector");
    assert_eq!(collector.run, RunKind::Async);
    assert_eq!(collector.shape(), NodeShape::Streaming);

    for c in &contracts {
        let hash = c.id.content_hash.as_deref().expect("every contract hashed");
        assert_eq!(hash.len(), 64);
    }
}

#[test]
fn demo_graph_compiles_and_aggregates_capabilities() {
    let contracts = fixtures();
    let compiled = demo_graph(&contracts)
        .compile(&contracts)
        .expect("demo graph is valid");

    assert!(compiled.warnings().is_empty(), "{}", compiled.warnings());
    let required = compiled.required_capabilities();
    let capabilities: Vec<(&str, Vec<(&str, &str)>)> = required
        .iter()
        .map(|c| {
            (
                c.interface.as_str(),
                c.items
                    .iter()
                    .flat_map(|(n, signatures)| signatures.iter().map(|s| (n.as_str(), s.as_str())))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        capabilities,
        [
            ("demo:caps/clock@0.1.0", vec![("now", "func()->u64")]),
            (
                "demo:caps/log@0.1.0",
                vec![("emit", r#"func("msg":string)"#)]
            ),
        ]
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
    assert!(position("archive") < position("collector"));
}

#[test]
fn demo_graph_partitions_into_stream_islands() {
    let contracts = fixtures();
    let compiled = demo_graph(&contracts).compile(&contracts).unwrap();
    let mut islands: Vec<BTreeSet<&str>> = compiled
        .islands()
        .iter()
        .map(|island| island.iter().map(|n| n.as_str()).collect())
        .collect();
    islands.sort();
    assert_eq!(
        islands,
        vec![
            BTreeSet::from(["accumulator"]),
            BTreeSet::from(["alarm", "display", "filter", "sensor"]),
            BTreeSet::from(["archive", "collector"]),
        ]
    );
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

    let diags = diagnostics(graph, &contracts);
    assert_eq!(diags.len(), 1, "{diags:?}");
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
    let spare = graph.nodes[0].clone();
    graph.nodes.push(witgraph_wit::ir::Node {
        id: "spare".into(),
        ..spare
    });
    let mut second = graph.connections[0].clone();
    second.id = "spare-view".into();
    second.from = PortRef::new("spare", "samples");
    second.to = PortRef::new("display", "view");
    graph.connections.push(second);

    let diags = diagnostics(graph, &contracts);
    assert_eq!(diags.len(), 1, "{diags:?}");
    match &diags[0] {
        Diagnostic::MultipleWriters { port, connections } => {
            assert_eq!(*port, PortRef::new("display", "view"));
            assert_eq!(connections.len(), 2);
        }
        other => panic!("expected MultipleWriters, got {other:?}"),
    }
}

#[test]
fn stream_fan_out_rejected() {
    let contracts = fixtures();
    let mut graph = demo_graph(&contracts);
    let mut tap = graph.connections[0].clone();
    tap.id = "tap".into();
    tap.from = PortRef::new("sensor", "samples");
    tap.to = PortRef::new("collector", "samples");
    graph.connections.retain(|c| c.id.as_str() != "collect");
    graph.connections.push(tap);

    let diags = diagnostics(graph, &contracts);
    assert!(
        diags.iter().any(|d| matches!(
            d,
            Diagnostic::AsyncFanOut { port, kind: PortKind::Stream, connections }
                if *port == PortRef::new("sensor", "samples") && connections.len() == 2
        )),
        "{diags:?}"
    );
}

#[test]
fn feedback_on_stream_rejected() {
    let contracts = fixtures();
    let mut graph = demo_graph(&contracts);
    let view = graph
        .connections
        .iter_mut()
        .find(|c| c.id.as_str() == "view")
        .unwrap();
    view.feedback = true;

    let diags = diagnostics(graph, &contracts);
    assert!(
        diags.iter().any(|d| matches!(
            d,
            Diagnostic::AsyncFeedback {
                kind: PortKind::Stream,
                ..
            }
        )),
        "{diags:?}"
    );
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

    let failure = graph.compile(&contracts).unwrap_err();
    assert!(failure.diagnostics.iter().any(|d| matches!(
        d,
        Diagnostic::IllegalCycle { nodes } if nodes.iter().any(|n| n.as_str() == "accumulator")
    )));
}

#[test]
fn serialized_graph_holds_refs_and_recompiles_against_rederived_contracts() {
    let contracts = fixtures();
    let graph = demo_graph(&contracts);

    let json = serde_json::to_value(&graph).unwrap();
    for entry in json["components"].as_array().unwrap() {
        let entry = entry.as_object().unwrap();
        assert!(
            entry.contains_key("content_hash"),
            "refs are pinned: {entry:?}"
        );
        assert!(
            !entry.contains_key("inputs") && !entry.contains_key("outputs"),
            "no contract data is serialized: {entry:?}"
        );
    }

    let restored: Graph = serde_json::from_value(json).unwrap();
    assert_eq!(graph, restored);
    restored
        .compile(&fixtures())
        .expect("round-tripped graph compiles against freshly lowered contracts");
}

#[test]
fn edited_component_no_longer_satisfies_a_pinned_graph() {
    let contracts = fixtures();
    let graph = demo_graph(&contracts);
    let mut edited = contracts.clone();
    let alarm = edited.iter_mut().find(|c| c.id.world == "alarm").unwrap();
    alarm.id.content_hash = Some("0".repeat(64));

    let diags = diagnostics(graph, &edited);
    assert!(
        diags.iter().any(|d| matches!(
            d,
            Diagnostic::ContractNotFound(id) if id.world == "alarm"
        )),
        "{diags:?}"
    );
}

#[test]
fn catalog_matches_committed_golden() {
    let catalog = metadata::generate_catalog(&load_lowered(FIXTURES).unwrap());
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
