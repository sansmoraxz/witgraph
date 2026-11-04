//! In-crate tests of the scheduler's internals: what only a test can set
//! up (an event a dropped tick left behind, a generation started between
//! two host writes), over an executor whose nodes are closures.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::executor::block_on;
use witgraph_ir::{
    CompiledGraph, ComponentContract, ComponentRef, FeedbackIdx, Graph, HandleType, InputIdx,
    NodeId, NodeIdx, PortDef, PortDirection, PortKind, PortName, PortRef, Type,
};

use crate::control::SchedulerCore;
use crate::executor::{
    BoxFuture, CallFuture, Canonical, Command, ControlEvent, ControlKind, DataPlane, Executor,
    Incarnation, OptionPayload, Outcome, Returned, Serving, boxed,
};
use crate::plan::{MemberPlan, RunShape};
use crate::{NodeFault, NodePhase, Perf, Scheduler, TickResult};

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
            PortDef::new("in", PortKind::Value, Type::U32).optional(),
            PortDef::new("add", PortKind::Value, Type::U32).optional(),
        ],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::U32)],
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

#[test]
fn what_a_dropped_tick_left_behind_is_handled_first() {
    let mut sched = load(Some("a"), &Arc::new(AtomicUsize::new(0)));
    // `a`'s `run` is in flight when the tick is dropped.
    assert!(sched.tick().now_or_never().is_none());
    let a = NodeId::from("a");
    assert_eq!(sched.node_state(&a).unwrap().phase(), NodePhase::Running);
    let index = sched.core.ports().node(&a).unwrap();
    let output = sched.core.ports().output(&a, &"out".into()).unwrap();
    // As if `a` returned during the dropped tick's last poll.
    sched
        .core
        .events_tx
        .unbounded_send(ControlEvent {
            incarnation: Incarnation::default(),
            kind: ControlKind::RunReturned {
                node: index,
                returned: Returned {
                    outputs_changed: vec![output],
                    ..Returned::default()
                },
            },
        })
        .unwrap();
    assert!(sched.take_faults().is_empty());
    assert_eq!(sched.node_state(&a).unwrap().phase(), NodePhase::Idle);
    assert!(sched.output_version(&a, &"out".into()).unwrap().get() > 0);
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
    assert!(matches!(block_on(sched.tick()), TickResult::Progress));
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
        vec![PortDef::new("add", PortKind::Value, Type::U32).optional()],
        vec![PortDef::handle("t", tensor())],
    );
    let take = contract(
        "take",
        vec![PortDef::handle("t", tensor())],
        vec![PortDef::new("out", PortKind::Value, Type::U32)],
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

/// A data plane a test answers by hand.
#[derive(Clone, Default)]
struct Scripted {
    serving: Arc<Mutex<Vec<ScriptedInstance>>>,
}

/// An instance of a [`Scripted`] plane.
type ScriptedInstance = (
    Incarnation,
    UnboundedReceiver<Command>,
    UnboundedSender<ControlEvent>,
);

impl DataPlane for Scripted {
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome> {
        self.serving
            .lock()
            .unwrap()
            .push((serving.incarnation, serving.commands, serving.events));
        boxed(futures::future::pending())
    }

    fn stopped(&self) {}
}

impl Scripted {
    /// What the live instance has been asked, in order.
    fn commands(&self) -> Vec<Command> {
        let mut serving = self.serving.lock().unwrap();
        let (_, commands, _) = serving.last_mut().unwrap();
        let mut asked = Vec::new();
        while let Ok(command) = commands.try_recv() {
            asked.push(command);
        }
        asked
    }

    fn report(&self, kind: ControlKind) {
        let serving = self.serving.lock().unwrap();
        let (incarnation, _, events) = serving.last().unwrap();
        events
            .unbounded_send(ControlEvent {
                incarnation: *incarnation,
                kind,
            })
            .unwrap();
    }

    fn returned(&self, node: NodeIdx) {
        self.report(ControlKind::RunReturned {
            node,
            returned: Returned::default(),
        });
    }
}

/// `p -> r` over a stream, `v` on its own, and `gpu` claims of `p` and `v` that
/// do not fit together.
fn stream_group(claims: bool) -> (SchedulerCore<Scripted>, Scripted) {
    let contract = |world: &str, inputs, outputs| ComponentContract {
        id: ComponentRef {
            content_hash: Some("cc".into()),
            ..(format!("test:nodes/{world}@0.1.0").parse().unwrap())
        },
        inputs,
        outputs,
        capabilities: vec![],
        docs: None,
    };
    let producer = contract(
        "producer",
        vec![],
        vec![PortDef::new("s", PortKind::Stream, Type::U32)],
    );
    let reader = contract(
        "reader",
        vec![PortDef::new("s", PortKind::Stream, Type::U32)],
        vec![],
    );
    let lone = contract("lone", vec![], vec![]);
    let contracts = [producer.clone(), reader.clone(), lone.clone()];
    let mut builder = Graph::builder("t")
        .add_component(&producer)
        .add_component(&reader)
        .add_component(&lone)
        .add_node("p", producer.id.clone())
        .add_node("r", reader.id.clone())
        .add_node("v", lone.id.clone())
        .connect("s", PortRef::new("p", "s"), PortRef::new("r", "s"));
    if claims {
        let claim = || witgraph_ir::ResourceClaim::new(0.6).unwrap();
        builder = builder
            .set_resource("p", "gpu", claim())
            .unwrap()
            .set_resource("v", "gpu", claim())
            .unwrap();
    }
    let compiled = builder.build().compile(&contracts).unwrap();
    let plane = Scripted::default();
    let core = SchedulerCore::new(compiled, Arc::new(Perf), 100, |_, _| plane.clone()).unwrap();
    (core, plane)
}

fn index(core: &SchedulerCore<Scripted>, node: &str) -> NodeIdx {
    core.ports().node(&node.into()).unwrap()
}

/// Polls a tick once, and drops it.
fn poll_tick(core: &mut SchedulerCore<dyn DataPlane>) -> Option<TickResult> {
    core.tick().now_or_never()
}

#[test]
fn a_group_re_runs_as_one_only_after_it_has_drained() {
    let (mut core, plane) = stream_group(false);
    let (p, r, v) = (index(&core, "p"), index(&core, "r"), index(&core, "v"));
    assert!(poll_tick(&mut core).is_none());
    // The reader's stream only exists once its producer returned.
    assert_eq!(plane.commands(), [Command::Run(p), Command::Run(v)]);
    plane.returned(p);
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(r)]);
    plane.returned(r);
    plane.returned(v);
    assert!(poll_tick(&mut core).is_none());
    let [Command::AwaitQuiet(at)] = plane.commands()[..] else {
        panic!("a tick ends when no guest work is left");
    };

    // Writers may be going on, so the group cannot start again yet.
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.rerun(&"r".into()).unwrap();
    assert!(core_dyn.tick().now_or_never().is_none());
    assert_eq!(plane.commands(), [], "still draining");

    // Quiet at last, so all of it runs, though only the reader owed one.
    plane.report(ControlKind::Quiet(at));
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(p)]);
    plane.returned(p);
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(r)]);
}

#[test]
fn a_value_node_beside_a_group_does_not_re_run_with_it() {
    let (mut core, plane) = stream_group(false);
    let (p, r, v) = (index(&core, "p"), index(&core, "r"), index(&core, "v"));
    assert!(poll_tick(&mut core).is_none());
    plane.commands();
    plane.returned(p);
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(r)]);
    plane.returned(r);
    plane.returned(v);
    assert!(poll_tick(&mut core).is_none());
    let [Command::AwaitQuiet(at)] = plane.commands()[..] else {
        panic!("a tick ends when no guest work is left");
    };
    plane.report(ControlKind::Quiet(at));
    assert!(poll_tick(&mut core).is_some(), "the tick is over");
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.rerun(&"p".into()).unwrap();
    assert!(core_dyn.tick().now_or_never().is_none());
    assert_eq!(plane.commands(), [Command::Run(p)]);
}

#[test]
fn a_group_holds_its_claims_until_it_has_drained() {
    let (mut core, plane) = stream_group(true);
    let (p, r, v) = (index(&core, "p"), index(&core, "r"), index(&core, "v"));
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(
        plane.commands(),
        [Command::Run(p)],
        "`v` does not fit beside the group"
    );
    plane.returned(p);
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(r)]);
    plane.returned(r);
    assert!(poll_tick(&mut core).is_none());
    let [Command::AwaitQuiet(at)] = plane.commands()[..] else {
        panic!("the group is draining");
    };
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [], "its writers may still use the claim");
    plane.report(ControlKind::Quiet(at));
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(v)]);
}

#[test]
fn nothing_starts_until_every_command_is_answered() {
    let contract = adder();
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .connect_feedback("loop", PortRef::new("n", "out"), PortRef::new("n", "in"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    let plane = Scripted::default();
    let mut core = SchedulerCore::new(compiled, Arc::new(Perf), 100, |_, _| plane.clone()).unwrap();
    let n = core.ports().node(&"n".into()).unwrap();
    let connection = core.ports().feedback_index(&"loop".into()).unwrap();
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(n)]);
    plane.report(ControlKind::RunReturned {
        node: n,
        returned: Returned {
            feedback_buffered: vec![connection],
            ..Returned::default()
        },
    });
    // Dropped, the tick latches as it ends: the iteration is over.
    assert!(poll_tick(&mut core).is_none());
    let commands = plane.commands();
    assert!(
        matches!(commands[..], [Command::AwaitQuiet(_), Command::LatchFeedback(c)] if c == connection),
        "{commands:?}"
    );
    // The tick waits on the answer; so does the next one, with `n` owing a run.
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.rerun(&"n".into()).unwrap();
    assert!(core_dyn.tick().now_or_never().is_none());
    assert_eq!(plane.commands(), []);
    plane.report(ControlKind::Latched {
        connection,
        latched: crate::Latched::Changed,
    });
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(n)]);
}

/// `s`, with a feedback connection from `s.out` into `s.in`, over a [`Scripted`]
/// plane: past its first run, with the latch of its feedback asked.
fn with_a_latch_asked() -> (SchedulerCore<Scripted>, Scripted, InputIdx, FeedbackIdx) {
    let looped = ComponentContract {
        id: ComponentRef {
            content_hash: Some("cc".into()),
            ..("test:nodes/looped@0.1.0".parse().unwrap())
        },
        inputs: vec![PortDef::new("in", PortKind::Value, Type::U32)],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::U32)],
        capabilities: vec![],
        docs: None,
    };
    let compiled = Graph::builder("t")
        .add_component(&looped)
        .add_node("s", looped.id.clone())
        .connect_feedback("ss", PortRef::new("s", "out"), PortRef::new("s", "in"))
        .build()
        .compile(std::slice::from_ref(&looped))
        .unwrap();
    let plane = Scripted::default();
    let mut core = SchedulerCore::new(compiled, Arc::new(Perf), 100, |_, _| plane.clone()).unwrap();
    let s = index(&core, "s");
    let input = core.ports().input(&"s".into(), &"in".into()).unwrap();
    let connection = core.ports().feedback_index(&"ss".into()).unwrap();
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.host_set(input);
    assert!(poll_tick(core_dyn).is_none());
    assert_eq!(plane.commands(), [Command::Run(s)]);
    plane.report(ControlKind::RunReturned {
        node: s,
        returned: Returned {
            feedback_buffered: vec![connection],
            ..Returned::default()
        },
    });
    // The tick ends here: it asks to latch what is over.
    assert!(poll_tick(core_dyn).is_none());
    let asked = plane.commands();
    assert!(
        asked.contains(&Command::LatchFeedback(connection)),
        "{asked:?}"
    );
    (core, plane, input, connection)
}

/// The lane serves the latch, then the clear: the clear has the last word, and the
/// node must not start on a value the hub no longer holds.
#[test]
fn a_clear_wins_over_a_latch_asked_before_it() {
    let (mut core, plane, input, connection) = with_a_latch_asked();
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.host_clear_started(input);
    plane.report(ControlKind::Latched {
        connection,
        latched: crate::Latched::Changed,
    });
    plane.report(ControlKind::HostCleared {
        input,
        cleared: true,
    });
    core_dyn.drain();
    assert!(!core_dyn.is_present(input));
    let _ = poll_tick(core_dyn);
    let s = core_dyn.ports().node(&"s".into()).unwrap();
    let asked = plane.commands();
    assert!(!asked.contains(&Command::Run(s)), "{asked:?}");
}

/// As above for a write: a latch that cleared the input has not the last word, and
/// the node must not wait for a value the hub holds.
#[test]
fn a_write_wins_over_a_latch_asked_before_it() {
    let (mut core, plane, input, connection) = with_a_latch_asked();
    let s = index(&core, "s");
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    core_dyn.host_write_started(input);
    plane.report(ControlKind::Latched {
        connection,
        latched: crate::Latched::Cleared,
    });
    plane.report(ControlKind::HostWrote {
        input,
        changed: true,
    });
    core_dyn.drain();
    assert!(core_dyn.is_present(input));
    let _ = poll_tick(core_dyn);
    let asked = plane.commands();
    assert!(asked.contains(&Command::Run(s)), "{asked:?}");
}
