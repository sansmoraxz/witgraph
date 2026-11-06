//! Stopping a guest written in C on its source lines, on wasmtime: as `tests/lines.rs` does a
//! Rust one.

#![cfg(feature = "debug")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use test_components::{C_WALK, Guest, LINE_WALK};
use witgraph_ir::{Graph, NodeId, PortName, PortRef};
use witgraph_runtime::{
    LineDebugger, LineStop, OwnLines, Perf, RunningGraph, RuntimeConfig, RuntimeGraph,
    RuntimeLines, StopReason, TickResult,
};

const SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/c-walk/src/walk.c");

/// The line of the `// mark: {name}` comment in `c-walk`'s source.
fn mark(name: &str) -> u32 {
    let wanted = format!("// mark: {name}");
    let at = SOURCE
        .lines()
        .position(|line| line.trim_end().ends_with(&wanted))
        .unwrap_or_else(|| panic!("no mark `{name}`"));
    u32::try_from(at).unwrap() + 1
}

/// `nodes` of the guests they name, `edges` between them from `out` to `in`.
async fn load(
    nodes: &[(&str, Guest)],
    edges: &[(&str, &str)],
) -> (RuntimeGraph<Perf>, RuntimeLines) {
    let mut contracts = Vec::new();
    let mut wasm = HashMap::new();
    let mut builder = Graph::builder("c");
    for (_, guest) in nodes {
        let contract = witgraph_wit::load_components(guest.wit).unwrap().remove(0);
        if !wasm.contains_key(&contract.id) {
            builder = builder.add_component(&contract);
            wasm.insert(contract.id.clone(), guest.wasm);
            contracts.push(contract);
        }
    }
    for (node, guest) in nodes {
        let contract = witgraph_wit::load_components(guest.wit).unwrap().remove(0);
        builder = builder.add_node(*node, contract.id);
    }
    for (from, to) in edges {
        builder = builder.connect(
            format!("{from}-{to}"),
            PortRef::new(*from, "out"),
            PortRef::new(*to, "in"),
        );
    }
    let config = RuntimeConfig {
        debug_lines: Some(OwnLines::default()),
        ..RuntimeConfig::default()
    };
    let compiled = builder.build().compile(&contracts).expect("graph compiles");
    let rt = RuntimeGraph::load(compiled, &wasm, config, Perf)
        .await
        .expect("graph loads");
    let lines = rt.lines().expect("loaded to debug");
    (rt, lines)
}

/// The C file, the one the guest has as its own: not its bindings, built without debug info.
fn c_file(lines: &RuntimeLines) -> String {
    let files: Vec<String> = lines
        .files()
        .into_iter()
        .filter(|file| file.ends_with(".c"))
        .collect();
    assert_eq!(files.len(), 1, "only the guest's own: {files:?}");
    assert!(files[0].ends_with("guests/c-walk/src/walk.c"), "{files:?}");
    files[0].clone()
}

async fn inject(rt: &mut RuntimeGraph<Perf>, node: &str, port: &str, wave: &str) {
    let (node, port) = (NodeId::from(node), PortName::from(port));
    rt.inject_wave(&node, &port, wave).await.unwrap();
}

async fn output(rt: &mut RuntimeGraph<Perf>, node: &str) -> Option<String> {
    let (node, port) = (NodeId::from(node), PortName::from("out"));
    rt.read_output_wave(&node, &port).await.unwrap()
}

/// Ticks until the graph is idle while `body` runs, with a limit on both.
async fn settling<T>(rt: &mut RuntimeGraph<Perf>, body: impl Future<Output = T>) -> T {
    let ticking = async {
        loop {
            match rt.tick().await {
                TickResult::Idle => break,
                TickResult::Progress | TickResult::StepLimitReached => {}
                other => panic!("a tick went wrong: {other:?}"),
            }
        }
    };
    let both = async { tokio::join!(ticking, body).1 };
    tokio::time::timeout(Duration::from_secs(60), both)
        .await
        .expect("it never finished")
}

fn variable<'a>(stop: &'a LineStop, name: &str) -> (&'a str, &'a str) {
    let found = stop
        .variables
        .iter()
        .find(|variable| variable.name == name)
        .unwrap_or_else(|| panic!("no `{name}` in {:?}", stop.variables));
    (found.type_name.as_str(), found.value.as_str())
}

#[tokio::test]
async fn a_breakpoint_stops_on_a_line_of_c_naming_the_node_and_function() {
    let (mut rt, lines) = load(&[("c", C_WALK)], &[]).await;
    let file = c_file(&lines);
    assert_eq!(
        lines.set_breakpoint(&file, mark("walk-first")),
        Some(mark("walk-first"))
    );
    inject(&mut rt, "c", "in", "5").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(stop.reason, StopReason::Breakpoint);
        assert_eq!(
            (stop.file.as_str(), stop.line),
            (file.as_str(), mark("walk-first"))
        );
        assert_eq!(stop.node.as_ref().map(|node| node.as_str()), Some("c"));
        let functions: Vec<&str> = stop
            .frames
            .iter()
            .map(|frame| frame.function.as_str())
            .collect();
        assert_eq!(functions[..2], ["walk", "node_run"], "{:?}", stop.frames);
        assert_eq!(
            stop.frames[1].line,
            Some(mark("run-walk")),
            "where it called `walk`"
        );
        assert_eq!(variable(&stop, "x"), ("uint32_t", "5"));
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "c").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn stepping_through_c_goes_over_a_call_into_one_and_out_of_it() {
    let (mut rt, lines) = load(&[("c", C_WALK)], &[]).await;
    let file = c_file(&lines);
    lines.set_breakpoint(&file, mark("walk-first"));
    inject(&mut rt, "c", "in", "5").await;
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        lines.clear_breakpoint(&file, mark("walk-first"));

        lines.step_over();
        let second = lines.next_stop().await;
        assert_eq!(
            (second.reason, second.line),
            (StopReason::Step, mark("walk-second"))
        );
        assert_eq!(variable(&second, "a"), ("uint32_t", "6"));
        assert_eq!(second.frames.len(), first.frames.len());

        lines.step_in();
        let inside = lines.next_stop().await;
        assert_eq!(inside.line, mark("add-body"));
        assert_eq!(inside.frames[0].function, "add", "{:?}", inside.frames);
        assert_eq!(variable(&inside, "a"), ("uint32_t", "6"));
        assert_eq!(variable(&inside, "b"), ("uint32_t", "2"));

        lines.step_out();
        let back = lines.next_stop().await;
        assert_eq!(back.frames[0].function, "walk", "{:?}", back.frames);
        assert_eq!(back.frames.len(), second.frames.len());
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "c").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn a_c_variable_of_each_kind_reads_as_c_would_write_it() {
    let (mut rt, lines) = load(&[("c", C_WALK)], &[]).await;
    let file = c_file(&lines);
    lines.set_breakpoint(&file, mark("kinds-end"));
    inject(&mut rt, "c", "kinds", "4").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        for (name, expected) in [
            ("seed", ("uint32_t", "4")),
            ("count", ("uint32_t", "5")),
            ("negative", ("int64_t", "-11")),
            ("ratio", ("double", "1.5")),
            ("flag", ("_Bool", "true")),
            ("letter", ("char", "120 'x'")),
            ("point", ("struct point", "{ .x = 3, .y = 2.5 }")),
            ("text", ("const char *", "\"hello\"")),
            ("list", ("uint32_t[3]", "{ 10, 20, 30 }")),
            ("shape", ("enum shape", "RECT")),
            ("bits", ("union bits", "{ .f = 1, .u = 1065353216 }")),
            ("pointer", ("struct point *", "→ { .x = 3, .y = 2.5 }")),
        ] {
            assert_eq!(variable(&stop, name), expected, "{name}");
        }
        let point = stop
            .variables
            .iter()
            .find(|variable| variable.name == "point")
            .unwrap();
        let fields: Vec<_> = point
            .children
            .iter()
            .map(|field| {
                (
                    field.name.as_str(),
                    field.type_name.as_str(),
                    field.value.as_str(),
                )
            })
            .collect();
        assert_eq!(fields, [("x", "int32_t", "3"), ("y", "double", "2.5")]);
        lines.resume();
    })
    .await;
    // `walk(0)`, and `count`.
    assert_eq!(output(&mut rt, "c").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn a_rust_guest_and_a_c_guest_are_debugged_side_by_side() {
    let (mut rt, lines) = load(&[("rust", LINE_WALK), ("c", C_WALK)], &[("rust", "c")]).await;
    let file = c_file(&lines);
    let files = lines.files();
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("guests/line-walk/src/lib.rs")),
        "{files:?}"
    );
    lines.set_breakpoint(&file, mark("walk-first"));
    inject(&mut rt, "rust", "in", "5").await;
    settling(&mut rt, async {
        // What the Rust guest made of 5 is what the C one walks.
        let stop = lines.next_stop().await;
        assert_eq!(stop.node.as_ref().map(|node| node.as_str()), Some("c"));
        assert_eq!(variable(&stop, "x"), ("uint32_t", "8"));
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "c").await.as_deref(), Some("11"));
}
