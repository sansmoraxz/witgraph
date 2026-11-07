//! Stopping a guest written in Zig on its source lines, on wasmtime: as `tests/c_lines.rs`
//! does a C one. Zig's debug info says it is C: its values are still written as Zig writes them.

#![cfg(feature = "debug")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use test_components::{C_WALK, Guest, LINE_WALK, ZIG_WALK};
use witgraph_ir::{Graph, NodeId, PortName, PortRef};
use witgraph_probes::{core_modules, source_lines};
use witgraph_runtime::{
    BreakpointScope, LineDebugger, LineStop, OwnLines, Perf, RunningGraph, RuntimeConfig,
    RuntimeGraph, RuntimeLines, StopReason, TickResult,
};

/// What the tests here set: written before breakpoints were per node, they ask for every one.
const EVERY: BreakpointScope = BreakpointScope::EveryNode;

const SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/zig-walk/src/walk.zig");
const SCALE_SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/zig-walk/src/scale.zig");

/// The line of the `// mark: {name}` comment in `zig-walk`'s `walk.zig`.
fn mark(name: &str) -> u32 {
    mark_in(SOURCE, name)
}

/// The line of the `// mark: {name}` comment in `zig-walk`'s `scale.zig`.
fn scale_mark(name: &str) -> u32 {
    mark_in(SCALE_SOURCE, name)
}

fn mark_in(source: &str, name: &str) -> u32 {
    let wanted = format!("// mark: {name}");
    let at = source
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
    load_with(nodes, edges, OwnLines::default()).await
}

/// [`load`], with `own` the lines that count.
async fn load_with(
    nodes: &[(&str, Guest)],
    edges: &[(&str, &str)],
    own: OwnLines,
) -> (RuntimeGraph<Perf>, RuntimeLines) {
    let mut contracts = Vec::new();
    let mut wasm = HashMap::new();
    let mut builder = Graph::builder("zig");
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
        debug_lines: Some(own),
        ..RuntimeConfig::default()
    };
    let compiled = builder.build().compile(&contracts).expect("graph compiles");
    let rt = RuntimeGraph::load(compiled, &wasm, config, Perf)
        .await
        .expect("graph loads");
    let lines = rt.lines().expect("loaded to debug");
    (rt, lines)
}

/// The Zig files the guest has as its own, `walk.zig` and `scale.zig`: not Zig's standard
/// library, nor its C bindings, built without debug info.
fn zig_files(lines: &RuntimeLines) -> (String, String) {
    let files: Vec<String> = lines
        .files()
        .into_iter()
        .filter(|file| file.ends_with(".zig"))
        .collect();
    let [scale, walk] = files.as_slice() else {
        panic!("only the guest's own: {files:?}");
    };
    assert!(walk.ends_with("guests/zig-walk/src/walk.zig"), "{files:?}");
    assert!(
        scale.ends_with("guests/zig-walk/src/scale.zig"),
        "{files:?}"
    );
    (walk.clone(), scale.clone())
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
async fn a_breakpoint_stops_on_a_line_of_zig_naming_the_node_and_function() {
    let (mut rt, lines) = load(&[("zig", ZIG_WALK)], &[]).await;
    let (file, _) = zig_files(&lines);
    assert_eq!(
        lines.set_breakpoint(&EVERY, &file, mark("walk-first")),
        Some(mark("walk-first"))
    );
    inject(&mut rt, "zig", "in", "5").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(stop.reason, StopReason::Breakpoint);
        assert_eq!(
            (stop.file.as_str(), stop.line),
            (file.as_str(), mark("walk-first"))
        );
        assert_eq!(stop.node.as_ref().map(|node| node.as_str()), Some("zig"));
        let functions: Vec<&str> = stop
            .frames
            .iter()
            .map(|frame| frame.function.as_str())
            .collect();
        assert_eq!(
            functions[..2],
            ["walk.walk", "walk.node_run"],
            "{:?}",
            stop.frames
        );
        assert_eq!(
            stop.frames[1].line,
            Some(mark("run-walk")),
            "where it called `walk`"
        );
        assert_eq!(variable(&stop, "x"), ("u32", "5"));
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "zig").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn only_the_guests_own_zig_is_its_own_not_zigs_standard_library() {
    let (_rt, lines) = load(&[("zig", ZIG_WALK)], &[]).await;
    zig_files(&lines);
    let (_rt, every) = load_with(&[("zig", ZIG_WALK)], &[], OwnLines::All).await;
    let files = every.files();
    assert!(
        files.iter().any(|file| file.ends_with("/std/mem.zig")),
        "every line is Zig's standard library's too: {files:?}"
    );
}

#[tokio::test]
async fn stepping_through_zig_goes_over_a_call_into_one_and_out_of_it() {
    let (mut rt, lines) = load(&[("zig", ZIG_WALK)], &[]).await;
    let (file, _) = zig_files(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    inject(&mut rt, "zig", "in", "5").await;
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        lines.clear_breakpoint(&EVERY, &file, mark("walk-first"));

        lines.step_over();
        let second = lines.next_stop().await;
        assert_eq!(
            (second.reason, second.line),
            (StopReason::Step, mark("walk-second"))
        );
        assert_eq!(variable(&second, "a"), ("u32", "6"));
        assert_eq!(second.frames.len(), first.frames.len());

        lines.step_in();
        let inside = lines.next_stop().await;
        assert_eq!(inside.line, mark("add-body"));
        assert_eq!(inside.frames[0].function, "walk.add", "{:?}", inside.frames);
        assert_eq!(variable(&inside, "a"), ("u32", "6"));
        assert_eq!(variable(&inside, "b"), ("u32", "2"));

        lines.step_out();
        let back = lines.next_stop().await;
        assert_eq!(back.frames[0].function, "walk.walk", "{:?}", back.frames);
        assert_eq!(back.frames.len(), second.frames.len());
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "zig").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn a_zig_variable_of_each_kind_reads_as_zig_would_write_it() {
    let (mut rt, lines) = load(&[("zig", ZIG_WALK)], &[]).await;
    let (file, _) = zig_files(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("kinds-end"));
    inject(&mut rt, "zig", "kinds", "4").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        for (name, type_name, value) in [
            ("count", "u32", "5"),
            ("negative", "i64", "-11"),
            ("ratio", "f64", "1.5"),
            ("flag", "bool", "true"),
            ("letter", "u8", "120"),
            // Zig lays a struct out as it likes: its fields come in that order.
            ("point", "walk.Point", ".{ .y = 2.5, .x = 3 }"),
            ("text", "[]const u8", "\"hello\""),
            ("list", "[3]u32", ".{ 10, 20, 30 }"),
            ("slice", "[]const u32", ".{ 10, 20, 30 }"),
            ("some", "?u32", "7"),
            ("none", "?u32", "null"),
            ("shape", "walk.Shape", ".{ .rect = .{ .w = 4, .h = 5 } }"),
            ("pointer", "*walk.Point", "→ .{ .y = 2.5, .x = 3 }"),
        ] {
            assert_eq!(variable(&stop, name), (type_name, value), "{name}");
        }
        lines.resume();
    })
    .await;
    // `walk(0)` is 3, and `kinds(4)` is 5.
    assert_eq!(output(&mut rt, "zig").await.as_deref(), Some("8"));
}

#[tokio::test]
async fn a_step_goes_from_one_zig_file_of_the_guest_into_another_and_back() {
    let (mut rt, lines) = load(&[("zig", ZIG_WALK)], &[]).await;
    let (walk, scale) = zig_files(&lines);
    lines.set_breakpoint(&EVERY, &walk, mark("run-scale"));
    inject(&mut rt, "zig", "scale", "3").await;
    settling(&mut rt, async {
        let at = lines.next_stop().await;
        assert_eq!(
            (at.file.as_str(), at.line),
            (walk.as_str(), mark("run-scale"))
        );
        lines.clear_breakpoint(&EVERY, &walk, mark("run-scale"));

        // Into `scale.zig`, called from `walk.zig`.
        lines.step_in();
        let into = lines.next_stop().await;
        assert_eq!(
            (into.file.as_str(), into.line),
            (scale.as_str(), scale_mark("scale-add"))
        );
        assert_eq!(into.frames[0].function, "scale.scale", "{:?}", into.frames);
        assert_eq!(into.frames[1].file.as_deref(), Some(walk.as_str()));
        assert_eq!(into.frames[1].line, Some(mark("run-scale")));
        assert_eq!(variable(&into, "x"), ("u32", "3"));

        // And from it back into `walk.zig`, a frame deeper.
        lines.step_in();
        let back = lines.next_stop().await;
        assert_eq!(
            (back.file.as_str(), back.line),
            (walk.as_str(), mark("add-body"))
        );
        assert_eq!(back.frames[1].file.as_deref(), Some(scale.as_str()));
        assert_eq!(back.frames.len(), into.frames.len() + 1);

        // Out of `add`, to `scale.zig` again, past the rest of the line of the call.
        lines.step_out();
        let out = lines.next_stop().await;
        assert_eq!(
            (out.file.as_str(), out.line),
            (scale.as_str(), scale_mark("scale-end"))
        );
        assert_eq!(variable(&out, "doubled"), ("u32", "6"));

        // Out of `scale`, to `walk.zig`.
        lines.step_out();
        let home = lines.next_stop().await;
        assert_eq!(home.file, walk, "{:?}", home.frames);
        assert_eq!(home.frames.len(), at.frames.len());
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "zig").await.as_deref(), Some("9"));
}

#[tokio::test]
async fn rust_c_and_zig_guests_are_debugged_side_by_side() {
    let (mut rt, lines) = load(
        &[("rust", LINE_WALK), ("c", C_WALK), ("zig", ZIG_WALK)],
        &[("rust", "c"), ("c", "zig")],
    )
    .await;
    let (walk, scale) = zig_files(&lines);
    assert_eq!(lines.files_of(&NodeId::from("zig")), [scale, walk.clone()]);
    lines.set_breakpoint(&EVERY, &walk, mark("walk-first"));
    inject(&mut rt, "rust", "in", "5").await;
    settling(&mut rt, async {
        // What the Rust guest made of 5, and the C one of that.
        let stop = lines.next_stop().await;
        assert_eq!(stop.node.as_ref().map(|node| node.as_str()), Some("zig"));
        assert_eq!(variable(&stop, "x"), ("u32", "11"));
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "zig").await.as_deref(), Some("14"));
}

/// Where wasmtime is asked to break for each of `guest`'s own lines, by file and line: offsets
/// from the start of its one core module with lines.
fn sites(guest: Guest) -> HashMap<(String, u32), Vec<u32>> {
    let mut with_lines = core_modules(guest.wasm)
        .unwrap()
        .into_iter()
        .map(|module| source_lines(module, &OwnLines::default()).unwrap())
        .filter(|probes| !probes.is_empty());
    let probes = with_lines.next().expect("a module with lines");
    assert!(with_lines.next().is_none(), "one module with lines");
    let mut sites: HashMap<(String, u32), Vec<u32>> = HashMap::new();
    for probe in probes {
        sites
            .entry((probe.file, probe.line))
            .or_default()
            .push(probe.offset.0);
    }
    sites
}

/// Ticks until the graph is idle, going on from every stop, and says where each was.
async fn run_through(rt: &mut RuntimeGraph<Perf>, lines: &RuntimeLines) -> Vec<LineStop> {
    let mut stops = Vec::new();
    let ticking = async {
        loop {
            match rt.tick().await {
                TickResult::Idle => break,
                TickResult::Progress | TickResult::StepLimitReached => {}
                other => panic!("a tick went wrong: {other:?}"),
            }
        }
    };
    let stopping = async {
        loop {
            stops.push(lines.next_stop().await);
            lines.resume();
        }
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::select! {
            () = ticking => {}
            () = stopping => {}
        }
    })
    .await
    .expect("it never finished");
    stops
}

/// The lines the C guest stops on, run a second time, given 5, with a breakpoint on every line of
/// its own. If `zig_too`, the Zig guest beside it has one on every line of its own as well, for
/// the first run of both, cleared before the second.
async fn c_stops(zig_too: bool) -> Vec<(String, u32)> {
    let (mut rt, lines) = load(&[("c", C_WALK), ("zig", ZIG_WALK)], &[]).await;
    let every = |node: &str| -> Vec<(String, u32)> {
        lines
            .files_of(&NodeId::from(node))
            .into_iter()
            .flat_map(|file| {
                let at = lines.lines(&file);
                at.into_iter().map(move |line| (file.clone(), line))
            })
            .collect()
    };
    let zig = if zig_too { every("zig") } else { Vec::new() };
    for (file, line) in every("c").iter().chain(&zig) {
        lines.set_breakpoint(&EVERY, file, *line);
    }
    // Stopping is what has the Store set them: until then, setting and clearing come to nothing.
    let first = run_through(&mut rt, &lines).await;
    let stopped = |node: &str| {
        first
            .iter()
            .any(|stop| stop.node == Some(NodeId::from(node)))
    };
    assert!(stopped("c"));
    assert_eq!(stopped("zig"), zig_too);
    for (file, line) in &zig {
        lines.clear_breakpoint(&EVERY, file, *line);
    }
    inject(&mut rt, "c", "in", "5").await;
    let second = run_through(&mut rt, &lines).await;
    assert_eq!(output(&mut rt, "c").await.as_deref(), Some("8"));
    second
        .into_iter()
        .map(|stop| {
            assert_eq!(stop.node, Some(NodeId::from("c")));
            (stop.file, stop.line)
        })
        .collect()
}

/// A composed graph's core modules are compiled as one: a breakpoint set and cleared in the Zig
/// guest's module must leave the C guest's at the same offset in its own.
#[tokio::test]
async fn clearing_one_guests_breakpoints_keeps_anothers_at_the_same_offset() {
    let alone = c_stops(false).await;
    assert!(!alone.is_empty());
    // Some line it stops on breaks only where the Zig guest also has a line, or this proves
    // nothing: clang's and zig's builds decide that.
    let c = sites(C_WALK);
    let zig: HashSet<u32> = sites(ZIG_WALK).into_values().flatten().collect();
    assert!(
        alone
            .iter()
            .any(|stop| c[stop].iter().all(|site| zig.contains(site))),
        "no line the C guest stops on shares all its offsets with the Zig guest's lines: \
         find two guests that do"
    );
    assert_eq!(c_stops(true).await, alone);
}
