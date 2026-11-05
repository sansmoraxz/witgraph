//! Stopping guests on source lines, on wasmtime: the same as `test/lines.test.ts` in
//! `packages/witgraph-loader` asks of a JavaScript host.

#![cfg(feature = "debug")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use test_components::{Guest, LINE_WALK, RELAY};
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, GraphBuilder, NodeId, PortName, PortRef,
};
use witgraph_runtime::{
    LineDebugger, LineStop, OwnLines, Perf, RunningGraph, RuntimeConfig, RuntimeGraph,
    RuntimeLines, StopReason, TickResult,
};

const SOURCE: &str =
    include_str!("../../../test-fixtures/test-components/guests/line-walk/src/lib.rs");

/// The line of the `// mark: {name}` comment in `line-walk`'s source.
fn mark(name: &str) -> u32 {
    let wanted = format!("// mark: {name}");
    let at = SOURCE
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

fn file_of(lines: &RuntimeLines) -> String {
    let files = lines.files();
    assert_eq!(files.len(), 1, "only the guest's own: {files:?}");
    assert!(
        files[0].ends_with("guests/line-walk/src/lib.rs"),
        "{files:?}"
    );
    files[0].clone()
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
        lines.set_breakpoint(&file, mark("walk-first")),
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

#[tokio::test]
async fn a_line_with_no_code_comes_to_the_next_that_has_it() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    let asked = mark("above-walk");
    let actual = lines.set_breakpoint(&file, asked).unwrap();
    assert!(actual > asked);
    assert!(lines.lines(&file).contains(&actual));
    assert!(!lines.lines(&file).contains(&asked));
    assert_eq!(
        lines.set_breakpoint(&file, 100_000),
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
    lines.set_breakpoint(&file, mark("walk-first"));
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(first.line, mark("walk-first"));
        lines.clear_breakpoint(&file, mark("walk-first"));

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
    lines.set_breakpoint(&file, mark("run-walk"));
    let seen = settling(&mut rt, async {
        let mut seen = vec![lines.next_stop().await.line];
        lines.clear_breakpoint(&file, mark("run-walk"));
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
    lines.set_breakpoint(&file, mark("walk-first"));
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

        lines.clear_breakpoint(&file, mark("walk-first"));
        lines.resume();
    })
    .await;
}

#[tokio::test]
async fn a_variable_of_each_kind_reads_as_its_source_would_write_it() {
    let (mut rt, lines) = line_walk(&["a"]).await;
    let file = file_of(&lines);
    lines.set_breakpoint(&file, mark("kinds-end"));
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
            lines.set_breakpoint(&file, mark("spin-body")),
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
        lines.set_breakpoint(&file, mark("walk-first"));
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
    lines.set_breakpoint(&file, mark("walk-first"));
    lines.clear_breakpoint(&file, mark("walk-first"));
    settling(&mut rt, async {
        tokio::time::sleep(Duration::from_millis(20)).await;
    })
    .await;
    assert!(lines.stopped().is_none());
    assert_eq!(output(&mut rt, "a", "out").await.as_deref(), Some("3"));

    // A new Store, with the breakpoint already set.
    lines.set_breakpoint(&file, mark("walk-second"));
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
    lines.set_breakpoint(&file, mark("walk-first"));
    settling(&mut rt, async {
        let first = lines.next_stop().await;
        assert_eq!(node_of(&first), Some("a"));
        lines.clear_breakpoint(&file, mark("walk-first"));
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
    lines.set_breakpoint(&file, mark("walk-first"));
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
    lines.set_breakpoint(&file, mark("add-body"));
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
    let guessed = load(OwnLines::Guess).await;
    assert_eq!(guessed.len(), 1);
    let directory = guessed[0].strip_suffix("lib.rs").unwrap().to_string();
    assert_eq!(load(OwnLines::Under(vec![directory])).await, guessed);
    assert!(
        load(OwnLines::Under(vec!["/nowhere/".to_string()]))
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
