//! The scheduler over an executor whose nodes are closures: what only a test can
//! set up (a generation started between two host writes), and what the plane does
//! with its values.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use futures::executor::block_on;
use witgraph_ir::{
    CompiledGraph, ComponentContract, ComponentRef, Graph, HandleType, NodeId, PortDef,
    PortDirection, PortName, PortRef, Type,
};

use witgraph_traits::{BoxFuture, NodeFault, NodePhase, Outcome, Perf, TickResult, boxed};

use crate::Scheduler;
use crate::executor::{CallFuture, Canonical, Executor, OptionPayload};
use crate::plan::{MemberPlan, RunShape};

/// Runs every node as `out = in + add + 1` (absent inputs read as 0). A
/// call of the `held` node never returns, and that many rebuilds fail.
struct Adders {
    held: Option<NodeId>,
    fail_rebuilds: Arc<AtomicUsize>,
    ports: HashSet<(PortRef, PortDirection)>,
}

impl Executor for Adders {
    type Value = u32;
    type Type = ();

    fn call(&self, member: Arc<MemberPlan>, args: Vec<Option<u32>>) -> CallFuture<u32> {
        let held = self.held.as_ref() == Some(&member.node);
        boxed(async move {
            if held {
                futures::future::pending::<()>().await;
            }
            let sum = args.iter().flatten().sum::<u32>() + 1;
            Ok(member
                .outputs
                .keys()
                .map(|port| (port.to_string(), sum))
                .collect())
        })
    }

    fn rebuild(&self) -> BoxFuture<Outcome> {
        let failing = self
            .fail_rebuilds
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        boxed(async move {
            if failing {
                let message = "no data".to_string();
                return Err((NodeFault::Restart { message }, None));
            }
            Ok(())
        })
    }

    fn close(&self, _: u32) {}

    fn abandon(&self, _: &[NodeId]) {}

    fn option_payload(_: &u32) -> OptionPayload<'_, u32> {
        OptionPayload::NotOption
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&()> {
        self.ports
            .contains(&(port.clone(), direction))
            .then_some(&())
    }

    fn check_input(&self, (): &(), value: u32) -> Result<Canonical<u32>, String> {
        Ok(Canonical::assume(value))
    }

    fn parse_wave(&self, (): &(), text: &str) -> Result<Canonical<u32>, String> {
        text.parse()
            .map(Canonical::assume)
            .map_err(|e| format!("{e}"))
    }

    fn to_wave(&self, value: &u32) -> Result<String, String> {
        Ok(value.to_string())
    }
}

/// `in: option<u32>`, `add: option<u32>` → `out: u32`.
fn adder() -> ComponentContract {
    ComponentContract {
        id: ComponentRef {
            content_hash: Some("aa".into()),
            ..("test:nodes/adder@0.1.0".parse().unwrap())
        },
        inputs: vec![
            PortDef::value("in", Type::U32).optional(),
            PortDef::value("add", Type::U32).optional(),
        ],
        outputs: vec![PortDef::value("out", Type::U32)],
        capabilities: vec![],
        docs: None,
    }
}

/// Loads `a → b` (`a.out` into `b.in`).
fn load(held: Option<&str>, fail_rebuilds: &Arc<AtomicUsize>) -> Scheduler<Perf, Adders> {
    let contract = adder();
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("a", contract.id.clone())
        .add_node("b", contract.id.clone())
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    load_adders(compiled, held, fail_rebuilds)
}

/// Loads a graph of [`adder`]s.
fn load_adders(
    compiled: CompiledGraph,
    held: Option<&str>,
    fail_rebuilds: &Arc<AtomicUsize>,
) -> Scheduler<Perf, Adders> {
    let mut ports = HashSet::new();
    let mut shapes: HashMap<NodeId, RunShape> = HashMap::new();
    let shape = RunShape {
        inputs: Some(vec!["in".into(), "add".into()]),
        has_result: true,
    };
    for node in compiled.topological_order() {
        for input in ["in", "add"] {
            ports.insert((PortRef::new(node.clone(), input), PortDirection::Input));
        }
        ports.insert((PortRef::new(node.clone(), "out"), PortDirection::Output));
        shapes.insert(node.clone(), shape.clone());
    }
    let executor = Adders {
        held: held.map(NodeId::from),
        fail_rebuilds: fail_rebuilds.clone(),
        ports,
    };
    Scheduler::new(compiled, Perf, executor, &shapes, 100).unwrap()
}

fn settle(sched: &mut Scheduler<Perf, Adders>) {
    for _ in 0..20 {
        match block_on(sched.tick()) {
            TickResult::Idle => return,
            TickResult::Progress => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("the graph never settled");
}

fn out(sched: &Scheduler<Perf, Adders>, node: &str) -> Option<u32> {
    sched.read_output(&node.into(), &"out".into()).unwrap()
}

/// `h`, whose call never returns, beside `n`, whose `out` feeds back into its
/// `add`, after a tick interrupted with the latch of `n`'s feedback queued.
fn interrupted_with_a_latch_queued() -> Scheduler<Perf, Adders> {
    let contract = adder();
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("h", contract.id.clone())
        .add_node("n", contract.id.clone())
        .connect_feedback("nn", PortRef::new("n", "out"), PortRef::new("n", "add"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    let mut sched = load_adders(compiled, Some("h"), &Arc::new(AtomicUsize::new(0)));
    let tick = block_on(sched.tick_until(std::future::ready(())));
    assert!(matches!(tick, TickResult::Interrupted), "{tick:?}");
    assert_eq!(out(&sched, "n"), Some(1));
    sched
}

#[test]
fn a_host_write_wins_over_a_latch_an_interrupted_tick_queued() {
    let mut sched = interrupted_with_a_latch_queued();
    sched.inject(&"n".into(), &"add".into(), 100).unwrap();
    assert!(sched.tick().now_or_never().is_none());
    assert_eq!(out(&sched, "n"), Some(101));
}

#[test]
fn a_host_clear_wins_over_a_latch_an_interrupted_tick_queued() {
    let mut sched = interrupted_with_a_latch_queued();
    sched.clear_input(&"n".into(), &"add".into()).unwrap();
    assert!(sched.tick().now_or_never().is_none());
    assert_eq!(out(&sched, "n"), Some(1));
}

#[test]
fn a_snapshot_leaves_out_feedback_a_host_write_dropped() {
    let contract = adder();
    // `m` waits for `h`, so `n`'s feedback into it stays buffered.
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("h", contract.id.clone())
        .add_node("m", contract.id.clone())
        .add_node("n", contract.id.clone())
        .connect("hm", PortRef::new("h", "out"), PortRef::new("m", "in"))
        .connect_feedback("nm", PortRef::new("n", "out"), PortRef::new("m", "add"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    let mut sched = load_adders(compiled, Some("h"), &Arc::new(AtomicUsize::new(0)));
    assert!(sched.tick().now_or_never().is_none());
    assert_eq!(sched.snapshot().unwrap().feedback.len(), 1, "buffered");
    sched.inject(&"m".into(), &"add".into(), 100).unwrap();
    assert!(sched.snapshot().unwrap().feedback.is_empty());
}

/// `n`, whose `out` feeds back into its `add`.
fn a_loop() -> Scheduler<Perf, Adders> {
    let contract = adder();
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .connect_feedback("nn", PortRef::new("n", "out"), PortRef::new("n", "add"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    load_adders(compiled, None, &Arc::new(AtomicUsize::new(0)))
}

#[test]
fn an_equal_host_write_makes_the_value_the_hosts() {
    let mut sched = a_loop();
    for _ in 0..2 {
        let _ = block_on(sched.tick());
    }
    assert_eq!(out(&sched, "n"), Some(2));
    assert!(sched.snapshot().unwrap().host_written.is_empty());
    let n = NodeId::from("n");
    // What the loop latched: changing nothing, it runs nothing.
    sched.inject(&n, &"add".into(), 2).unwrap();
    let written = sched.snapshot().unwrap().host_written;
    assert_eq!(written[&n].len(), 1, "{written:?}");
    sched.shutdown();
    let add = sched.core.ports().input(&n, &"add".into()).unwrap();
    assert!(
        sched.core.is_present(add),
        "the next instance is written it"
    );
}

#[test]
fn a_restored_loop_starts_over_with_the_next_instance() {
    let mut sched = a_loop();
    for _ in 0..3 {
        let _ = block_on(sched.tick());
    }
    let looped = out(&sched, "n");
    assert!(looped > Some(1), "{looped:?}");
    let snapshot = sched.snapshot().unwrap();
    assert!(snapshot.host_written.is_empty());
    sched.restore(&snapshot).unwrap();
    assert_eq!(
        snapshot.inputs[&NodeId::from("n")].len(),
        1,
        "what it latched"
    );
    let _ = block_on(sched.tick());
    sched.shutdown();
    sched.rerun(&"n".into()).unwrap();
    assert!(sched.tick().now_or_never().is_some());
    assert_eq!(out(&sched, "n"), Some(1), "a loop starts over");
}

#[test]
fn a_restore_keeps_what_the_host_wrote_for_the_next_instance() {
    let mut sched = a_loop();
    let _ = block_on(sched.tick());
    sched.inject(&"n".into(), &"in".into(), 40).unwrap();
    let _ = block_on(sched.tick());
    let snapshot = sched.snapshot().unwrap();
    sched.restore(&snapshot).unwrap();
    let _ = block_on(sched.tick());
    sched.shutdown();
    sched.rerun(&"n".into()).unwrap();
    assert!(sched.tick().now_or_never().is_some());
    assert_eq!(out(&sched, "n"), Some(41));
}

#[test]
fn a_snapshot_may_not_say_the_host_wrote_a_connected_input() {
    let mut sched = load(None, &Arc::new(AtomicUsize::new(0)));
    settle(&mut sched);
    let mut snapshot = sched.snapshot().unwrap();
    snapshot
        .host_written
        .entry("b".into())
        .or_default()
        .insert("in".into());
    assert!(matches!(
        sched.restore(&snapshot),
        Err(crate::RuntimeError::SnapshotMismatch { .. })
    ));
}

#[test]
fn work_owed_during_a_failing_rebuild_survives_it() {
    let fail = Arc::new(AtomicUsize::new(0));
    let mut sched = load(None, &fail);
    settle(&mut sched);
    let a = NodeId::from("a");
    sched.shutdown();
    fail.store(1, Ordering::SeqCst);
    sched.inject(&a, &"add".into(), 1).unwrap();
    // The rebuild starts, then an input changes before it fails.
    assert!(sched.core().rebuild_if_owed());
    sched.inject(&a, &"add".into(), 2).unwrap();
    // The attempt fails; the change it never saw is still owed, so the
    // graph is rebuilt again (successfully, now) and runs on it.
    settle(&mut sched);
    let faults = sched.take_faults();
    assert!(
        matches!(faults.as_slice(), [report] if matches!(report.fault, NodeFault::Restart { .. })),
        "the first rebuild failed: {faults:?}"
    );
    assert_eq!(sched.node_state(&a).unwrap().phase(), NodePhase::Idle);
    assert_eq!(out(&sched, "a"), Some(3), "and the graph ran on the change");
}

#[test]
fn a_failed_rebuild_is_not_retried_until_something_new_is_owed() {
    let fail = Arc::new(AtomicUsize::new(0));
    let mut sched = load(None, &fail);
    settle(&mut sched);
    assert_eq!(out(&sched, "b"), Some(2));

    let a = NodeId::from("a");
    fail.store(1, Ordering::SeqCst);
    sched.shutdown();
    sched.inject(&a, &"add".into(), 1).unwrap();
    assert!(matches!(block_on(sched.tick()), TickResult::Faulted(_)));
    assert!(matches!(
        sched.node_state(&a).unwrap().fault_cause(),
        Some(NodeFault::Restart { message }) if message.contains("no data")
    ));
    assert_eq!(out(&sched, "b"), None);
    assert!(matches!(block_on(sched.tick()), TickResult::Idle));

    sched.inject(&a, &"add".into(), 5).unwrap();
    settle(&mut sched);
    assert_eq!(out(&sched, "b"), Some(5 + 1 + 1));
}

fn tensor() -> HandleType {
    HandleType {
        interface: "test:data/tensor@0.1.0".into(),
        resource: "tensor".into(),
    }
}

/// `make`: `add: option<u32>` → `t: own<tensor>`; `take`: `t:
/// borrow<tensor>` → `out: u32`; loaded as `a → b` (`a.t` into `b.t`). The
/// adders' `u32`s stand in for handles.
fn load_handles() -> Scheduler<Perf, Adders> {
    let contract = |world: &str, inputs, outputs| ComponentContract {
        id: ComponentRef {
            content_hash: Some("bb".into()),
            ..(format!("test:nodes/{world}@0.1.0").parse().unwrap())
        },
        inputs,
        outputs,
        capabilities: vec![],
        docs: None,
    };
    let make = contract(
        "make",
        vec![PortDef::value("add", Type::U32).optional()],
        vec![PortDef::handle("t", tensor())],
    );
    let take = contract(
        "take",
        vec![PortDef::handle("t", tensor())],
        vec![PortDef::value("out", Type::U32)],
    );
    let contracts = [make.clone(), take.clone()];
    let compiled = Graph::builder("t")
        .add_component(&make)
        .add_component(&take)
        .add_node("a", make.id.clone())
        .add_node("b", take.id.clone())
        .connect("ab", PortRef::new("a", "t"), PortRef::new("b", "t"))
        .build()
        .compile(&contracts)
        .unwrap();
    // Only Value ports have a type.
    let ports = HashSet::from([
        (PortRef::new("a", "add"), PortDirection::Input),
        (PortRef::new("b", "out"), PortDirection::Output),
    ]);
    let shapes: HashMap<NodeId, RunShape> = ["a", "b"]
        .into_iter()
        .map(|node| {
            let shape = RunShape {
                inputs: Some(vec![if node == "a" { "add" } else { "t" }.into()]),
                has_result: true,
            };
            (NodeId::from(node), shape)
        })
        .collect();
    let executor = Adders {
        held: None,
        fail_rebuilds: Arc::new(AtomicUsize::new(0)),
        ports,
    };
    Scheduler::new(compiled, Perf, executor, &shapes, 100).unwrap()
}

#[test]
fn handles_are_delivered_and_read_like_values() {
    let mut sched = load_handles();
    assert!(
        sched.compiled().stream_groups().is_empty(),
        "a handle, unlike a stream, joins no group"
    );
    sched.inject(&"a".into(), &"add".into(), 3).unwrap();
    settle(&mut sched);
    assert_eq!(
        sched.read_output(&"a".into(), &"t".into()).unwrap(),
        Some(4)
    );
    assert_eq!(out(&sched, "b"), Some(5));
    assert!(
        matches!(
            sched.inject(&"b".into(), &"t".into(), 1),
            Err(crate::RuntimeError::NotAValuePort { .. })
        ),
        "a handle is not text the host can write"
    );
}

#[test]
fn snapshots_leave_handles_out_and_rerun_their_producers() {
    let mut sched = load_handles();
    sched.inject(&"a".into(), &"add".into(), 3).unwrap();
    settle(&mut sched);
    let snapshot = sched.snapshot().unwrap();
    let ports = |values: &crate::PortValues| -> Vec<String> {
        values
            .iter()
            .flat_map(|(node, ports)| ports.keys().map(move |port| format!("{node}.{port}")))
            .collect()
    };
    assert_eq!(ports(&snapshot.inputs), ["a.add"]);
    assert_eq!(ports(&snapshot.outputs), ["b.out"]);
    assert_eq!(
        snapshot.owed,
        [NodeId::from("a")].into_iter().collect(),
        "the producer owes a run"
    );

    let mut restored = load_handles();
    restored.restore(&snapshot).unwrap();
    assert_eq!(
        restored.read_output(&"a".into(), &"t".into()).unwrap(),
        None
    );
    settle(&mut restored);
    assert_eq!(
        restored.read_output(&"a".into(), &"t".into()).unwrap(),
        Some(4),
        "the producer made its handle anew"
    );
    assert_eq!(out(&restored, "b"), Some(5));
}

#[test]
fn a_version_moves_when_a_run_writes_the_port() {
    let mut sched = load(None, &Arc::new(AtomicUsize::new(0)));
    let (a, port) = (NodeId::from("a"), PortName::from("out"));
    assert_eq!(sched.output_version(&a, &port).unwrap().get(), 0);
    settle(&mut sched);
    let first = sched.output_version(&a, &port).unwrap();
    assert!(first.get() > 0);
    // Nothing ran: the same version, and nothing to read.
    assert!(matches!(block_on(sched.tick()), TickResult::Idle));
    assert_eq!(sched.output_version(&a, &port).unwrap(), first);
    sched.inject(&a, &"add".into(), 7).unwrap();
    settle(&mut sched);
    assert!(sched.output_version(&a, &port).unwrap() > first);
    assert!(matches!(
        sched.output_version(&a, &"in".into()),
        Err(crate::RuntimeError::NotAValuePort { .. })
    ));
    assert!(matches!(
        sched.output_version(&"nobody".into(), &port),
        Err(crate::RuntimeError::UnknownNode { .. })
    ));
}

#[test]
fn a_restore_moves_every_version() {
    let mut sched = load(None, &Arc::new(AtomicUsize::new(0)));
    settle(&mut sched);
    let (a, port) = (NodeId::from("a"), PortName::from("out"));
    let before = sched.output_version(&a, &port).unwrap();
    let snapshot = sched.snapshot().unwrap();
    sched.restore(&snapshot).unwrap();
    assert!(sched.output_version(&a, &port).unwrap() > before);
}

#[test]
fn a_reader_reads_while_a_tick_holds_the_scheduler() {
    let mut sched = load(Some("b"), &Arc::new(AtomicUsize::new(0)));
    let reader = sched.reader();
    let (a, out) = (NodeId::from("a"), PortName::from("out"));
    {
        // `b` never returns, so the tick stays in flight, holding `sched`.
        let tick = sched.tick();
        futures::pin_mut!(tick);
        assert!(tick.as_mut().now_or_never().is_none());
        assert_eq!(reader.read_output(&a, &out).unwrap(), Some(1));
        assert!(reader.output_version(&a, &out).unwrap().get() > 0);
    }
}
