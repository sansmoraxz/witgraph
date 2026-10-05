//! In-crate tests of the scheduler's internals: what only a test can set
//! up (an event a dropped tick left behind, a generation started between
//! two host writes), over an executor whose nodes are closures.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use futures::executor::block_on;
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, NodeId, PortDef, PortDirection, PortKind, PortName,
    PortRef, Type,
};

use crate::executor::{
    Executor, Generation, GenerationFuture, IslandEvent, IslandEventKind, NodeCaller,
    OptionPayload, drive,
};
use crate::plan::RunShape;
use crate::{NodeFault, NodePhase, Perf, Scheduler, TickResult};

/// Runs every node as `out = in + add + 1` (absent inputs read as 0). A
/// call of the `held` node never returns, and rebuilds fail while
/// `fail_rebuilds` is set.
struct Adders {
    held: Option<NodeId>,
    fail_rebuilds: Arc<AtomicBool>,
    ports: HashSet<(PortRef, PortDirection)>,
}

struct Members<'a> {
    generation: &'a Generation<u32>,
    held: Option<&'a NodeId>,
}

impl NodeCaller<u32> for Members<'_> {
    type Error = String;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<u32>>,
    ) -> Result<Vec<(String, u32)>, String> {
        if self.held == Some(&self.generation.plan.members[member].node) {
            futures::future::pending::<()>().await;
        }
        let sum = args.iter().flatten().sum::<u32>() + 1;
        Ok(vec![("out".into(), sum)])
    }

    fn close(&self, _: u32) -> Result<(), String> {
        Ok(())
    }

    fn missing_input(&self, _: usize, field: &PortName) -> String {
        format!("no value for `{field}`")
    }
}

impl Adders {
    fn generation(&self, mut generation: Generation<u32>) -> GenerationFuture<()> {
        let held = self.held.clone();
        async move {
            let external = std::mem::take(&mut generation.external);
            let members = Members {
                generation: &generation,
                held: held.as_ref(),
            };
            let send = |kind| generation.send(kind);
            drive(
                &members,
                &generation.plan,
                external,
                Self::option_payload,
                &send,
            )
            .await
            .map_err(|message| (NodeFault::WasmTrap { message }, None))
        }
        .boxed()
    }
}

impl Executor for Adders {
    type Value = u32;
    type Type = ();
    type Island = ();

    fn run(&self, (): (), generation: Generation<u32>) -> GenerationFuture<()> {
        self.generation(generation)
    }

    fn rebuild_and_run(&self, generation: Generation<u32>) -> GenerationFuture<()> {
        if self.fail_rebuilds.load(Ordering::SeqCst) {
            let fault = NodeFault::Restart {
                message: "no data".into(),
            };
            return async move { Err((fault, None)) }.boxed();
        }
        generation.send(IslandEventKind::Rebuilt);
        self.generation(generation)
    }

    fn option_payload(_: &u32) -> OptionPayload<'_, u32> {
        OptionPayload::NotOption
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&()> {
        self.ports
            .contains(&(port.clone(), direction))
            .then_some(&())
    }

    fn check_input(&self, (): &(), value: u32) -> Result<u32, String> {
        Ok(value)
    }

    fn parse_wave(&self, (): &(), text: &str) -> Result<u32, String> {
        text.parse().map_err(|e| format!("{e}"))
    }

    fn to_wave(&self, value: &u32) -> String {
        value.to_string()
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
fn load(held: Option<&str>, fail_rebuilds: &Arc<AtomicBool>) -> Scheduler<Perf, Adders> {
    let contract = adder();
    let compiled = Graph::builder("t")
        .add_component(&contract)
        .add_node("a", contract.id.clone())
        .add_node("b", contract.id.clone())
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build()
        .compile(std::slice::from_ref(&contract))
        .unwrap();
    let mut ports = HashSet::new();
    for node in ["a", "b"] {
        for input in ["in", "add"] {
            ports.insert((PortRef::new(node, input), PortDirection::Input));
        }
        ports.insert((PortRef::new(node, "out"), PortDirection::Output));
    }
    let shape = RunShape {
        inputs: Some(vec!["in".into(), "add".into()]),
        has_result: true,
    };
    let islands = compiled
        .islands()
        .iter()
        .map(|members| ((), vec![shape.clone(); members.len()]))
        .collect();
    let executor = Adders {
        held: held.map(NodeId::from),
        fail_rebuilds: fail_rebuilds.clone(),
        ports,
    };
    Scheduler::new(compiled, Perf, executor, islands, 100).unwrap()
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
fn cancel_handles_events_a_dropped_tick_left_behind() {
    let mut sched = load(Some("a"), &Arc::new(AtomicBool::new(false)));
    // `a`'s generation is in flight when the tick is dropped.
    assert!(sched.tick().now_or_never().is_none());
    let a = NodeId::from("a");
    let island = sched.compiled().island_of(&a).unwrap();
    let slot = &sched.slots[island];
    let generation = slot.state.running_generation().unwrap();
    let member = slot.plan.members.iter().position(|m| m.node == a).unwrap();
    // As if `a` returned during the dropped tick's last poll.
    let _ = sched.events_tx.unbounded_send(IslandEvent {
        island,
        generation,
        kind: IslandEventKind::RunReturned {
            member,
            values: vec![("out".into(), 5)],
        },
    });

    sched.cancel(&a).unwrap();
    assert_eq!(out(&sched, "a"), Some(5));
    // `b` is not held: it runs on what `a` returned.
    sched.executor_mut().held = None;
    assert!(matches!(block_on(sched.tick()), TickResult::Progress));
    assert_eq!(
        out(&sched, "b"),
        Some(6),
        "the returned run's outputs were delivered"
    );
}

#[test]
fn work_owed_during_a_failing_rebuild_survives_it() {
    let fail = Arc::new(AtomicBool::new(false));
    let mut sched = load(None, &fail);
    settle(&mut sched);
    let a = NodeId::from("a");
    sched.cancel(&a).unwrap();
    fail.store(true, Ordering::SeqCst);
    sched.inject(&a, &"add".into(), 1).unwrap();
    // The rebuild starts, then an input changes before it fails.
    let island = sched.compiled().island_of(&a).unwrap();
    sched.start_generation(island);
    sched.inject(&a, &"add".into(), 2).unwrap();
    // The attempt fails; the change it never saw is still owed, so the
    // island is rebuilt again (successfully, now) and runs on it.
    fail.store(false, Ordering::SeqCst);
    settle(&mut sched);
    let faults = sched.take_faults();
    assert!(
        matches!(faults.as_slice(), [report] if matches!(report.fault, NodeFault::Restart { .. })),
        "the first rebuild failed: {faults:?}"
    );
    assert_eq!(sched.node_state(&a).unwrap().phase(), NodePhase::Idle);
    assert_eq!(
        out(&sched, "a"),
        Some(3),
        "and the island ran on the change"
    );
}

#[test]
fn a_failed_rebuild_frees_downstream_in_the_same_tick() {
    let fail = Arc::new(AtomicBool::new(false));
    let mut sched = load(None, &fail);
    settle(&mut sched);
    assert_eq!(out(&sched, "b"), Some(2));

    let a = NodeId::from("a");
    fail.store(true, Ordering::SeqCst);
    sched.cancel(&a).unwrap();
    sched.inject(&a, &"add".into(), 1).unwrap();
    sched.inject(&"b".into(), &"add".into(), 5).unwrap();
    // `a` comes first and holds `b` back until its rebuild (the start of
    // its generation) fails; then `b` runs in the same tick.
    sched.set_max_steps_per_tick(2).unwrap();

    assert!(matches!(block_on(sched.tick()), TickResult::Progress));
    assert_eq!(out(&sched, "b"), Some(1 + 5 + 1));
    assert!(matches!(
        sched.node_state(&a).unwrap().fault_cause(),
        Some(NodeFault::Restart { message }) if message.contains("no data")
    ));
}
