//! Stopping guests on source lines, on wasmtime: the same as `test/lines.test.ts` in
//! `packages/witgraph-loader` asks of a JavaScript host.

#![cfg(feature = "debug")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use test_components::{
    Guest, LINE_STREAM, LINE_STREAM_SINK, LINE_WALK, RELAY, STREAM_DRAIN, STREAM_PRODUCER,
};
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, GraphBuilder, NodeId, PortName, PortRef,
};
use witgraph_runtime::{
    BreakpointScope, LineDebugger, LineStop, OwnLines, Perf, RunningGraph, RuntimeConfig,
    RuntimeGraph, RuntimeLines, StopReason, TickResult,
};

/// What the tests here set: written before breakpoints were per node, they ask for every one.
const EVERY: BreakpointScope = BreakpointScope::EveryNode;

const SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/line-walk/src/lib.rs");
const SCALE_SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/line-walk/src/scale.rs");

/// The line of the `// mark: {name}` comment in `line-walk`'s `lib.rs`.
fn mark(name: &str) -> u32 {
    mark_in(SOURCE, name)
}

/// The line of the `// mark: {name}` comment in `line-walk`'s `scale.rs`.
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

struct Kit {
    contracts: Vec<ComponentContract>,
    wasm: HashMap<ComponentRef, &'static [u8]>,
}

impl Kit {
    fn new(guests: &[Guest]) -> Self {
        let mut kit = Kit {
            contracts: Vec::new(),
            wasm: HashMap::new(),
        };
        for guest in guests {
            let mut contracts = witgraph_wit::load_components(guest.wit).unwrap();
            let contract = contracts.remove(0);
            kit.wasm.insert(contract.id.clone(), guest.wasm);
            kit.contracts.push(contract);
        }
        kit
    }

    fn id(&self, at: usize) -> ComponentRef {
        self.contracts[at].id.clone()
    }

    fn builder(&self) -> GraphBuilder {
        self.contracts
            .iter()
            .fold(Graph::builder("lines"), |builder, contract| {
                builder.add_component(contract)
            })
    }

    async fn load(&self, graph: Graph) -> (RuntimeGraph<Perf>, RuntimeLines) {
        let config = RuntimeConfig {
            debug_lines: Some(OwnLines::default()),
            ..RuntimeConfig::default()
        };
        let compiled = graph.compile(&self.contracts).expect("graph compiles");
        let rt = RuntimeGraph::load(compiled, &self.wasm, config, Perf)
            .await
            .expect("graph loads");
        let lines = rt.lines().expect("loaded to debug");
        (rt, lines)
    }
}

async fn line_walk(nodes: &[&str]) -> (RuntimeGraph<Perf>, RuntimeLines) {
    let kit = Kit::new(&[LINE_WALK]);
    let mut builder = kit.builder();
    for node in nodes {
        builder = builder.add_node(*node, kit.id(0));
    }
    kit.load(builder.build()).await
}

/// The files `line-walk` has as its own, `lib.rs` and `scale.rs`.
fn files_of(lines: &RuntimeLines) -> (String, String) {
    let files = lines.files();
    let [main, scale] = files.as_slice() else {
        panic!("only the guest's own: {files:?}");
    };
    assert!(main.ends_with("guests/line-walk/src/lib.rs"), "{files:?}");
    assert!(
        scale.ends_with("guests/line-walk/src/scale.rs"),
        "{files:?}"
    );
    (main.clone(), scale.clone())
}

/// `lib.rs`, of [`files_of`].
fn file_of(lines: &RuntimeLines) -> String {
    files_of(lines).0
}

async fn inject(rt: &mut RuntimeGraph<Perf>, node: &str, port: &str, wave: &str) {
    let (node, port) = (NodeId::from(node), PortName::from(port));
    rt.inject_wave(&node, &port, wave).await.unwrap();
}

async fn output(rt: &mut RuntimeGraph<Perf>, node: &str, port: &str) -> Option<String> {
    let (node, port) = (NodeId::from(node), PortName::from(port));
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

fn value<'a>(stop: &'a LineStop, name: &str) -> &'a str {
    let found = stop.variables.iter().find(|variable| variable.name == name);
    &found
        .unwrap_or_else(|| panic!("no `{name}` in {:?}", stop.variables))
        .value
}

fn node_of(stop: &LineStop) -> Option<&str> {
    stop.node.as_ref().map(|node| node.as_str())
}

fn has(stop: &LineStop, name: &str) -> bool {
    stop.variables.iter().any(|variable| variable.name == name)
}

#[tokio::test]
async fn a_breakpoint_stops_on_its_line_naming_the_node_and_resuming_goes_on() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    assert_eq!(
        lines.set_breakpoint(&EVERY, &file, mark("walk-first")),
        Some(mark("walk-first"))
    );
    inject(&mut rt, "a", "in", "5").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(stop.reason, StopReason::Breakpoint);
        assert_eq!(stop.file, file);
        assert_eq!(stop.line, mark("walk-first"));
        assert_eq!(node_of(&stop), Some("a"));
        assert_eq!(
            stop.frames[0].function, "line_walk_guest::walk",
            "{:?}",
            stop.frames
        );
        assert_eq!(lines.stopped().as_ref(), Some(&stop));

        // Held for as long as it takes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(lines.stopped().is_some());
        lines.resume();
        assert!(lines.stopped().is_none());
    })
    .await;
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("8"));
}

/// How many stops a breakpoint on `line` of `file` has, with the guest's input already written.
async fn count_stops(
    rt: &mut RuntimeGraph<Perf>,
    lines: &RuntimeLines,
    file: &str,
    line: u32,
) -> usize {
    assert_eq!(lines.set_breakpoint(&EVERY, file, line), Some(line));
    let mut stops = 0;
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
            let stop = lines.next_stop().await;
            assert_eq!((stop.file.as_str(), stop.line), (file, line));
            stops += 1;
            lines.resume();
        }
    };
    let both = async {
        tokio::select! {
            () = ticking => {}
            () = stopping => {}
        }
    };
    tokio::time::timeout(Duration::from_secs(60), both)
        .await
        .expect("it never finished");
    stops
}

/// How many stops `line-walk` has on `name`, with `port` written.
async fn stops_on(name: &str, port: &str, wave: &str) -> usize {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    inject(&mut rt, "a", port, wave).await;
    count_stops(&mut rt, &lines, &file, mark(name)).await
}

#[tokio::test]
async fn a_breakpoint_stops_once_for_each_time_its_line_runs() {
    assert_eq!(stops_on("walk-first", "in", "5").await, 1);
    assert_eq!(stops_on("spin-body", "spin", "6").await, 6);
}

/// How many stops a stream guest has on the line marked `name`, reading 4 items.
async fn stream_stops(guest: Guest, source: &str, name: &str, sink: Option<Guest>) -> usize {
    let wanted = format!("// mark: {name}");
    let line = source
        .lines()
        .position(|line| line.trim_end().ends_with(&wanted))
        .unwrap();
    let line = u32::try_from(line).unwrap() + 1;
    let mut guests = vec![STREAM_PRODUCER, guest];
    guests.extend(sink);
    let kit = Kit::new(&guests);
    let mut builder = kit
        .builder()
        .add_node("source", kit.id(0))
        .add_node("guest", kit.id(1))
        .connect(
            "in",
            PortRef::new("source", "items"),
            PortRef::new("guest", "items"),
        );
    if sink.is_some() {
        builder = builder.add_node("sink", kit.id(2)).connect(
            "out",
            PortRef::new("guest", "items"),
            PortRef::new("sink", "items"),
        );
    }
    let (mut rt, lines) = kit.load(builder.build()).await;
    let file = lines
        .files()
        .into_iter()
        .find(|file| file.contains("/line-stream"))
        .expect("the guest has its own lines");
    inject(&mut rt, "source", "burst-size", "4").await;
    count_stops(&mut rt, &lines, &file, line).await
}

const STREAM_SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/line-stream/src/lib.rs");
const SINK_SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/line-stream-sink/src/lib.rs");

#[tokio::test]
async fn a_breakpoint_in_a_stream_loop_stops_for_every_item() {
    // A line that awaits runs again when it is polled again, so can stop more than once for
    // an item, but never less. One that does not stops exactly once.
    for (name, at_least) in [("read", 5), ("write", 4)] {
        let stops = stream_stops(LINE_STREAM, STREAM_SOURCE, name, Some(STREAM_DRAIN)).await;
        assert!(
            stops >= at_least,
            "writing on: {name} stopped {stops} times"
        );
    }
    let body = stream_stops(LINE_STREAM, STREAM_SOURCE, "body", Some(STREAM_DRAIN)).await;
    assert_eq!(body, 4, "writing on");
    let read = stream_stops(LINE_STREAM_SINK, SINK_SOURCE, "read", None).await;
    assert!(read >= 5, "reading in `run`: stopped {read} times");
    let body = stream_stops(LINE_STREAM_SINK, SINK_SOURCE, "body", None).await;
    assert_eq!(body, 4, "reading in `run`");
}

#[tokio::test]
async fn a_breakpoint_still_stops_after_a_step() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("spin-body"));
    inject(&mut rt, "a", "spin", "3").await;
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(first.line, mark("spin-body"));
        lines.step_over();
        let stepped = lines.next_stop().await;
        assert_ne!(stepped.line, mark("spin-body"));
        lines.resume();
        let again = lines.next_stop().await;
        assert_eq!(again.line, mark("spin-body"));
        assert_eq!(again.reason, StopReason::Breakpoint);
        lines.clear_breakpoint(&EVERY, &file, mark("spin-body"));
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_line_with_no_code_comes_to_the_next_that_has_it() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    let asked = mark("above-walk");
    let actual = lines.set_breakpoint(&EVERY, &file, asked).unwrap();
    assert!(actual > asked);
    assert!(lines.lines(&file).contains(&actual));
    assert!(!lines.lines(&file).contains(&asked));
    assert_eq!(
        lines.set_breakpoint(&EVERY, &file, 100_000),
        None,
        "past the last line"
    );
    settling(&mut rt, async {
        assert_eq!(lines.next_stop().await.line, actual);
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn stepping_goes_over_a_call_into_one_and_out_of_it() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(first.line, mark("walk-first"));
        lines.clear_breakpoint(&EVERY, &file, mark("walk-first"));

        lines.step_over();
        let second = lines.next_stop().await;
        assert_eq!(second.reason, StopReason::Step);
        assert_eq!(second.line, mark("walk-second"), "over the call of `add`");
        assert_eq!(second.frames.len(), first.frames.len());

        lines.step_in();
        let inside = lines.next_stop().await;
        assert_eq!(
            inside.line,
            mark("add-body"),
            "past its opening line, to where its parameters are",
        );
        assert_eq!(
            inside.frames[0].function, "line_walk_guest::add",
            "{:?}",
            inside.frames
        );
        assert!(inside.frames.len() > second.frames.len(), "a frame deeper");

        lines.step_out();
        let back = lines.next_stop().await;
        assert_eq!(
            back.frames[0].function, "line_walk_guest::walk",
            "{:?}",
            back.frames
        );
        assert_eq!(back.frames.len(), second.frames.len());
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_step_never_stops_in_what_a_macro_generated() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("run-walk"));
    let seen = settling(&mut rt, async {
        let mut seen = vec![lines.next_stop().await.line];
        lines.clear_breakpoint(&EVERY, &file, mark("run-walk"));
        for _ in 0..40 {
            lines.step_in();
            let stop = tokio::time::timeout(Duration::from_millis(500), lines.next_stop()).await;
            match stop {
                Ok(stop) => seen.push(stop.line),
                Err(_) => break,
            }
        }
        lines.release();
        seen
    })
    .await;
    assert!(seen.contains(&mark("add-body")), "{seen:?}");
    assert!(!seen.contains(&1), "the `generate!` line: {seen:?}");
}

#[tokio::test]
async fn a_stop_says_what_its_variables_hold_and_which_are_in_scope() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    inject(&mut rt, "a", "in", "5").await;
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(value(&first, "x"), "5");
        let x = first.variables.iter().find(|variable| variable.name == "x");
        assert_eq!(x.unwrap().type_name, "u32");
        assert!(!has(&first, "a"), "not yet");

        lines.step_over();
        let second = lines.next_stop().await;
        assert_eq!(value(&second, "a"), "6", "its value, now it has one");

        lines.step_in();
        let body = lines.next_stop().await;
        assert_eq!(value(&body, "a"), "6", "the callee's own `a`");
        assert_eq!(value(&body, "b"), "2");
        assert!(!has(&body, "x"), "its caller's is out of scope");

        lines.clear_breakpoint(&EVERY, &file, mark("walk-first"));
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_variable_of_each_kind_reads_as_its_source_would_write_it() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("kinds-end"));
    inject(&mut rt, "a", "kinds", "4").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(stop.line, mark("kinds-end"));
        for (name, expected) in [
            ("seed", "4"),
            ("count", "5"),
            ("negative", "-11"),
            ("ratio", "1.5"),
            ("flag", "true"),
            ("letter", "'x'"),
            ("point", "Point { x: 3, y: 2.5 }"),
            ("text", "\"hello\""),
            ("owned", "\"owned\""),
            ("list", "[10, 20, 30]"),
            ("slice", "[10, 20, 30]"),
            ("array", "[1, 2, 3]"),
            ("some", "Some(7)"),
            ("none", "None"),
            ("dot", "Dot"),
            ("circle", "Circle(0.5)"),
            ("rect", "Rect { w: 4, h: 5 }"),
            ("pair", "(1, 2)"),
            ("reference", "→ Point { x: 3, y: 2.5 }"),
            ("boxed", "→ 9"),
        ] {
            assert_eq!(value(&stop, name), expected, "{name}");
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
        assert_eq!(fields, [("x", "i32", "3"), ("y", "f64", "2.5")]);
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn pausing_stops_a_guest_in_the_middle_of_a_spin() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    inject(&mut rt, "a", "spin", "4000000000").await;
    let (end, ended) = futures::channel::oneshot::channel::<()>();
    let ticking = rt.tick_until(async {
        let _ = ended.await;
    });
    let body = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        lines.pause();
        let stop = lines.next_stop().await;
        assert_eq!(stop.reason, StopReason::Pause);
        assert_eq!(stop.file, file);
        assert_eq!(node_of(&stop), Some("a"));
        assert!(
            stop.frames[0].function.contains("spin"),
            "{:?}",
            stop.frames
        );

        // A breakpoint set while it spins is seen too, with nothing else reaching the Store.
        lines.resume();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            lines.set_breakpoint(&EVERY, &file, mark("spin-body")),
            Some(mark("spin-body"))
        );
        let hit = lines.next_stop().await;
        assert_eq!(hit.reason, StopReason::Breakpoint);
        assert_eq!(hit.line, mark("spin-body"));
        lines.release();
        let _ = end.send(());
    };
    let both = async { tokio::join!(ticking, body) };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(60), both)
        .await
        .expect("it never finished");
    assert!(matches!(result, TickResult::Interrupted), "{result:?}");
    rt.shutdown();
}

#[tokio::test]
async fn a_stop_says_whose_guest_it_is_whichever_order_the_graph_has_them_in() {
    for [first, second] in [["a", "b"], ["b", "a"]] {
        let kit = Kit::new(&[LINE_WALK]);
        // Whichever is declared first, `a` feeds `b`.
        let graph = kit
            .builder()
            .add_node(first, kit.id(0))
            .add_node(second, kit.id(0))
            .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .build();
        let (mut rt, lines) = kit.load(graph).await;
        let file = file_of(&lines);
        lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
        inject(&mut rt, "a", "in", "5").await;
        settling(&mut rt, async {
            for (node, x) in [("a", "5"), ("b", "8")] {
                let stop = lines.next_stop().await;
                assert_eq!(node_of(&stop), Some(node));
                assert_eq!(value(&stop, "x"), x, "{node}");
                lines.resume();
            }
        })
        .await;
    }
}

#[tokio::test]
async fn a_cleared_breakpoint_does_not_stop_and_one_set_stays_through_a_rebuild() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    lines.clear_breakpoint(&EVERY, &file, mark("walk-first"));
    settling(&mut rt, async {
        tokio::time::sleep(Duration::from_millis(20)).await;
    })
    .await;
    assert!(lines.stopped().is_none());
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("3"));

    // A new Store, with the breakpoint already set.
    lines.set_breakpoint(&EVERY, &file, mark("walk-second"));
    rt.shutdown();
    rt.rerun(&NodeId::from("a")).unwrap();
    settling(&mut rt, async {
        assert_eq!(lines.next_stop().await.line, mark("walk-second"));
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_step_stays_with_its_guest_while_the_others_run_past() {
    let kit = Kit::new(&[LINE_WALK]);
    let graph = kit
        .builder()
        .add_node("a", kit.id(0))
        .add_node("b", kit.id(0))
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let (mut rt, lines) = kit.load(graph).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(node_of(&first), Some("a"));
        lines.clear_breakpoint(&EVERY, &file, mark("walk-first"));
        for _ in 0..30 {
            lines.step_over();
            match tokio::time::timeout(Duration::from_millis(400), lines.next_stop()).await {
                Ok(stop) => {
                    assert_eq!(node_of(&stop), Some("a"), "only its own");
                }
                Err(_) => break,
            }
        }
        lines.release();
    })
    .await;
    assert_eq!(output(&mut rt, "b", "out").await.as_deref(), Some("6"));
}

#[tokio::test]
async fn shutting_down_ends_a_guest_stopped_on_a_line() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("walk-first"));
    let (end, ended) = futures::channel::oneshot::channel::<()>();
    let ticking = rt.tick_until(async {
        let _ = ended.await;
    });
    let body = async {
        lines.next_stop().await;
        let _ = end.send(());
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let (result, ()) = tokio::join!(ticking, body);
        assert!(matches!(result, TickResult::Interrupted), "{result:?}");
    })
    .await
    .expect("stopping a tick does not wait for the guest");
    assert!(lines.stopped().is_some(), "still held");
    rt.shutdown();
    assert!(lines.stopped().is_none(), "gone with its Store");

    // And a new Store runs it again, to the same line.
    rt.rerun(&NodeId::from("a")).unwrap();
    settling(&mut rt, async {
        assert_eq!(lines.next_stop().await.line, mark("walk-first"));
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_frame_says_the_line_it_is_waiting_on() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&EVERY, &file, mark("add-body"));
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(stop.frames[0].function, "line_walk_guest::add");
        assert_eq!(stop.frames[0].line, Some(mark("add-body")));
        assert_eq!(stop.frames[0].file.as_deref(), Some(file.as_str()));
        assert_eq!(
            stop.frames[1].function, "line_walk_guest::walk",
            "{:?}",
            stop.frames
        );
        assert_eq!(
            stop.frames[1].line,
            Some(mark("walk-first")),
            "the call it made"
        );
        assert_eq!(stop.frames[1].file.as_deref(), Some(file.as_str()));
        lines.release();
    })
    .await;
}

#[tokio::test]
async fn a_host_says_which_lines_are_its_own() {
    let kit = Kit::new(&[LINE_WALK]);
    let load = async |own: OwnLines| {
        let config = RuntimeConfig {
            debug_lines: Some(own),
            ..RuntimeConfig::default()
        };
        let graph = kit.builder().add_node("a", kit.id(0)).build();
        let compiled = graph.compile(&kit.contracts).unwrap();
        let rt = RuntimeGraph::load(compiled, &kit.wasm, config, Perf)
            .await
            .unwrap();
        rt.lines().unwrap().files()
    };
    let guessed = load(OwnLines::default()).await;
    assert_eq!(guessed.len(), 2, "`lib.rs` and `scale.rs`: {guessed:?}");
    let directory = guessed[0].strip_suffix("lib.rs").unwrap().to_string();
    assert_eq!(load(OwnLines::under([directory])).await, guessed);
    assert!(
        load(OwnLines::under(["/nowhere/".to_string()]))
            .await
            .is_empty()
    );
    assert!(
        load(OwnLines::All).await.len() > 1,
        "the standard library's too"
    );
}

#[tokio::test]
async fn a_guest_with_no_debug_info_has_no_lines_and_runs() {
    let kit = Kit::new(&[RELAY]);
    let graph = kit
        .builder()
        .add_node("r", kit.id(0))
        .add_node("s", kit.id(0))
        .connect_feedback("loop", PortRef::new("r", "out"), PortRef::new("r", "in"))
        .connect("rs", PortRef::new("r", "out"), PortRef::new("s", "in"))
        .build();
    let (mut rt, lines) = kit.load(graph).await;
    assert_eq!(lines.files(), Vec::<String>::new());
    inject(&mut rt, "r", "in", "2").await;
    settling(&mut rt, async {}).await;
    assert_eq!(output(&mut rt, "s", "out").await.as_deref(), Some("2"));
}

#[tokio::test]
async fn an_engine_not_made_to_debug_is_refused_and_a_graph_may_share_one_that_is() {
    let kit = Kit::new(&[LINE_WALK]);
    let graph = || kit.builder().add_node("a", kit.id(0)).build();

    let plain = RuntimeConfig::default().new_engine().unwrap();
    let config = RuntimeConfig {
        debug_lines: Some(OwnLines::default()),
        engine: Some(plain),
        ..RuntimeConfig::default()
    };
    let compiled = graph().compile(&kit.contracts).unwrap();
    let refused = RuntimeGraph::load(compiled, &kit.wasm, config, Perf).await;
    let error = refused
        .err()
        .expect("it cannot stop on anything")
        .to_string();
    assert!(error.contains("debug_lines"), "{error}");

    // A graph that does not ask to debug still runs on an engine that can.
    let debugging = RuntimeConfig {
        debug_lines: Some(OwnLines::default()),
        ..RuntimeConfig::default()
    };
    let config = RuntimeConfig {
        engine: Some(debugging.new_engine().unwrap()),
        ..RuntimeConfig::default()
    };
    let compiled = graph().compile(&kit.contracts).unwrap();
    let mut rt = RuntimeGraph::load(compiled, &kit.wasm, config, Perf)
        .await
        .unwrap();
    assert!(rt.lines().is_none());
    settling(&mut rt, async {}).await;
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("3"));
}

#[tokio::test]
async fn a_step_goes_from_one_file_of_the_guest_into_another_and_back() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let (main, scale) = files_of(&lines);
    assert_eq!(
        lines.files_of(&NodeId::from("a")),
        [main.clone(), scale.clone()]
    );
    lines.set_breakpoint(&EVERY, &main, mark("run-scale"));
    inject(&mut rt, "a", "scale", "3").await;
    settling(&mut rt, async {
        let at = lines.next_stop().await;
        assert_eq!(
            (at.file.as_str(), at.line),
            (main.as_str(), mark("run-scale"))
        );
        lines.clear_breakpoint(&EVERY, &main, mark("run-scale"));

        // Into `scale.rs`, called from `lib.rs`.
        lines.step_in();
        let into = lines.next_stop().await;
        assert_eq!(
            (into.file.as_str(), into.line),
            (scale.as_str(), scale_mark("scale-add"))
        );
        assert_eq!(
            into.frames[0].function, "line_walk_guest::scale::scale",
            "{:?}",
            into.frames
        );
        assert_eq!(into.frames[0].file.as_deref(), Some(scale.as_str()));
        assert_eq!(
            into.frames[1].file.as_deref(),
            Some(main.as_str()),
            "{:?}",
            into.frames
        );
        assert_eq!(into.frames[1].line, Some(mark("run-scale")));
        assert_eq!(value(&into, "x"), "3");

        // And from it back into `lib.rs`, a frame deeper.
        lines.step_in();
        let back = lines.next_stop().await;
        assert_eq!(
            (back.file.as_str(), back.line),
            (main.as_str(), mark("add-body"))
        );
        assert_eq!(
            back.frames[0].function, "line_walk_guest::add",
            "{:?}",
            back.frames
        );
        assert_eq!(back.frames[1].file.as_deref(), Some(scale.as_str()));
        assert_eq!(back.frames[1].line, Some(scale_mark("scale-add")));
        assert_eq!(back.frames.len(), into.frames.len() + 1);

        // Out of `add`, to `scale.rs` again, and on to its next line.
        lines.step_out();
        let out = lines.next_stop().await;
        assert_eq!(out.file, scale, "{:?}", out.frames);
        assert_eq!(out.frames[0].function, "line_walk_guest::scale::scale");
        assert_eq!(
            out.line,
            scale_mark("scale-end"),
            "past the rest of the line of the call"
        );
        assert_eq!(value(&out, "doubled"), "6");
        assert_eq!(out.frames.len(), into.frames.len());

        // Out of `scale`, to `lib.rs`.
        lines.step_out();
        let home = lines.next_stop().await;
        assert_eq!(home.file, main, "{:?}", home.frames);
        assert_eq!(home.frames.len(), at.frames.len(), "{:?}", home.frames);
        lines.resume();
    })
    .await;
    // `walk(0)` is 3, and `scale(3)` is 6.
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("9"));
}

#[tokio::test]
async fn a_breakpoint_in_a_second_file_stops_there() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let (_, scale) = files_of(&lines);
    lines.set_breakpoint(&EVERY, &scale, scale_mark("scale-end"));
    inject(&mut rt, "a", "scale", "4").await;
    settling(&mut rt, async {
        let stop = lines.next_stop().await;
        assert_eq!(
            (stop.file.as_str(), stop.line),
            (scale.as_str(), scale_mark("scale-end"))
        );
        assert_eq!(node_of(&stop), Some("a"));
        assert_eq!(value(&stop, "doubled"), "8");
        lines.resume();
    })
    .await;
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("11"));
}

/// The nodes that stop on `walk-first`, in order, in a graph of `a` feeding `b`, with a
/// breakpoint set there for each of `set` and those of `cleared` cleared again.
async fn who_stops(set: &[BreakpointScope], cleared: &[BreakpointScope]) -> Vec<String> {
    let kit = Kit::new(&[LINE_WALK]);
    let graph = kit
        .builder()
        .add_node("a", kit.id(0))
        .add_node("b", kit.id(0))
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let (mut rt, lines) = kit.load(graph).await;
    let file = file_of(&lines);
    for scope in set {
        assert_eq!(
            lines.set_breakpoint(scope, &file, mark("walk-first")),
            Some(mark("walk-first"))
        );
    }
    for scope in cleared {
        lines.clear_breakpoint(scope, &file, mark("walk-first"));
    }
    inject(&mut rt, "a", "in", "5").await;
    let mut stopped = Vec::new();
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
            let stop = lines.next_stop().await;
            stopped.push(node_of(&stop).unwrap().to_string());
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
    assert_eq!(output(&mut rt, "b", "out").await.as_deref(), Some("11"));
    stopped
}

#[tokio::test]
async fn a_breakpoint_for_a_node_stops_only_it_and_one_for_every_node_all_of_them() {
    use BreakpointScope::{EveryNode, Node};
    let (a, b) = (Node(NodeId::from("a")), Node(NodeId::from("b")));
    assert_eq!(who_stops(std::slice::from_ref(&b), &[]).await, ["b"]);
    assert_eq!(who_stops(std::slice::from_ref(&a), &[]).await, ["a"]);
    assert_eq!(who_stops(&[EveryNode], &[]).await, ["a", "b"]);
    assert_eq!(who_stops(&[a.clone(), b.clone()], &[]).await, ["a", "b"]);
    assert!(
        who_stops(&[Node(NodeId::from("nowhere"))], &[])
            .await
            .is_empty()
    );
    // One for a node and one for every node are two: each is cleared on its own.
    assert_eq!(
        who_stops(&[a.clone(), EveryNode], &[EveryNode]).await,
        ["a"]
    );
    assert_eq!(
        who_stops(&[a.clone(), EveryNode], std::slice::from_ref(&a)).await,
        ["a", "b"]
    );
    assert!(
        who_stops(std::slice::from_ref(&b), std::slice::from_ref(&b))
            .await
            .is_empty()
    );
}
