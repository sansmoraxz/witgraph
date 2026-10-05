#![cfg(test)]
#![allow(missing_docs)]

//! The scheduler over an executor with no engine in it: nodes are Rust
//! closures and values are numbers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::FutureExt;
use futures::executor::block_on;
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, NodeId, PortDef, PortDirection, PortKind, PortName,
    PortRef, Type,
};
use witgraph_sched::{
    Executor, Generation, GenerationFuture, IslandEventKind, NodeCaller, NodeFault, NodePhase,
    OptionPayload, Perf, RunShape, RuntimeError, RuntimeMode, Scheduler, TickResult, Trace,
    TraceEvent, drive,
};

/// What a node computes: its inputs, by field, to its outputs, by name.
type Behaviour = Arc<dyn Fn(&[Option<u32>]) -> Result<Vec<(String, u32)>, String> + Send + Sync>;

/// Runs nodes as closures. An island has no state between generations, so
/// its live island is `()`.
struct Closures {
    behaviour: Arc<HashMap<NodeId, Behaviour>>,
    ports: HashSet<(PortRef, PortDirection)>,
    rebuilds: Arc<AtomicUsize>,
}

struct Members<'a> {
    generation: &'a Generation<u32>,
    behaviour: &'a HashMap<NodeId, Behaviour>,
}

impl NodeCaller<u32> for Members<'_> {
    type Error = String;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<u32>>,
    ) -> Result<Vec<(String, u32)>, String> {
        let node = &self.generation.plan.members[member].node;
        (self.behaviour[node])(&args)
    }

    fn close(&self, _: u32) -> Result<(), String> {
        Ok(())
    }

    fn missing_input(&self, _: usize, field: &PortName) -> String {
        format!("no value for `{field}`")
    }
}

impl Closures {
    fn generation(&self, mut generation: Generation<u32>) -> GenerationFuture<()> {
        let behaviour = self.behaviour.clone();
        async move {
            let external = std::mem::take(&mut generation.external);
            let members = Members {
                generation: &generation,
                behaviour: &behaviour,
            };
            let send = |kind| generation.send(kind);
            drive(
                &members,
                &generation.plan,
                external,
                Closures::option_payload,
                &send,
            )
            .await
            .map_err(|message| (NodeFault::WasmTrap { message }, None))
        }
        .boxed()
    }
}

impl Executor for Closures {
    type Value = u32;
    type Type = ();
    type Island = ();

    fn run(&self, (): (), generation: Generation<u32>) -> GenerationFuture<()> {
        self.generation(generation)
    }

    fn rebuild_and_run(&self, generation: Generation<u32>) -> GenerationFuture<()> {
        self.rebuilds.fetch_add(1, Ordering::SeqCst);
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

/// `in: option<u32>` → `out: u32`.
fn contract(world: &str) -> ComponentContract {
    ComponentContract {
        id: ComponentRef {
            content_hash: Some("aa".into()),
            ..format!("mock:nodes/{world}@0.1.0").parse().unwrap()
        },
        inputs: vec![PortDef::new("in", PortKind::Value, Type::U32).optional()],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::U32)],
        capabilities: vec![],
        docs: None,
    }
}

struct Loaded<M: RuntimeMode> {
    sched: Scheduler<M, Closures>,
    rebuilds: Arc<AtomicUsize>,
}

/// Loads `graph`, whose every node adds one to its input (absent reads as
/// 0); a node named in `failing` fails while its flag is set.
fn load<M: RuntimeMode>(
    graph: Graph,
    mode: M,
    failing: Option<(&str, Arc<AtomicBool>)>,
) -> Loaded<M> {
    let compiled = graph.compile(&[contract("inc")]).expect("compiles");
    let mut behaviour: HashMap<NodeId, Behaviour> = HashMap::new();
    let mut ports = HashSet::new();
    for node in &compiled.graph().nodes {
        let fail = failing
            .as_ref()
            .filter(|(name, _)| node.id.as_str() == *name)
            .map(|(_, flag)| flag.clone());
        behaviour.insert(
            node.id.clone(),
            Arc::new(move |args: &[Option<u32>]| {
                if fail.as_ref().is_some_and(|f| f.load(Ordering::SeqCst)) {
                    return Err("boom".to_string());
                }
                Ok(vec![("out".into(), args[0].unwrap_or(0) + 1)])
            }),
        );
        ports.insert((PortRef::new(node.id.clone(), "in"), PortDirection::Input));
        ports.insert((PortRef::new(node.id.clone(), "out"), PortDirection::Output));
    }
    let shape = RunShape {
        inputs: Some(vec!["in".into()]),
        has_result: true,
    };
    let islands = compiled
        .islands()
        .iter()
        .map(|members| ((), vec![shape.clone(); members.len()]))
        .collect();
    let rebuilds = Arc::new(AtomicUsize::new(0));
    let executor = Closures {
        behaviour: Arc::new(behaviour),
        ports,
        rebuilds: rebuilds.clone(),
    };
    Loaded {
        sched: Scheduler::new(compiled, mode, executor, islands, 100).expect("loads"),
        rebuilds,
    }
}

fn builder() -> witgraph_ir::GraphBuilder {
    Graph::builder("mock").add_component(inc())
}

fn inc() -> ComponentRef {
    contract("inc").id
}

fn out<M: RuntimeMode>(sched: &Scheduler<M, Closures>, node: &str) -> Option<u32> {
    sched.read_output(&node.into(), &"out".into()).unwrap()
}

fn settle<M: RuntimeMode>(sched: &mut Scheduler<M, Closures>) -> usize {
    for ticks in 1..=20 {
        match block_on(sched.tick()) {
            TickResult::Idle => return ticks,
            TickResult::Progress => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("the graph never settled");
}

#[test]
fn values_flow_down_a_chain_and_equal_values_do_not_rerun() {
    let graph = builder()
        .add_node("a", inc())
        .add_node("b", inc())
        .connect("c", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let Loaded { mut sched, .. } = load(graph, Trace::new(), None);
    sched.inject(&"a".into(), &"in".into(), 5).unwrap();
    settle(&mut sched);
    assert_eq!(out(&sched, "a"), Some(6));
    assert_eq!(out(&sched, "b"), Some(7));

    let runs = |trace: &[TraceEvent]| {
        trace
            .iter()
            .filter(|e| matches!(e, TraceEvent::RunStarted { .. }))
            .count()
    };
    assert_eq!(runs(&sched.mode().take_trace()), 2);
    // The same value again is not a change; another one runs both.
    sched.inject(&"a".into(), &"in".into(), 5).unwrap();
    settle(&mut sched);
    assert_eq!(runs(&sched.mode().take_trace()), 0);
    sched.inject_wave(&"a".into(), &"in".into(), "9").unwrap();
    settle(&mut sched);
    assert_eq!(out(&sched, "b"), Some(11));
    assert_eq!(runs(&sched.mode().take_trace()), 2);
}

#[test]
fn a_feedback_loop_advances_one_iteration_per_tick() {
    let graph = builder()
        .add_node("n", inc())
        .connect_feedback("loop", PortRef::new("n", "out"), PortRef::new("n", "in"))
        .build();
    let Loaded { mut sched, .. } = load(graph, Perf, None);
    for iteration in 1..=4 {
        assert!(matches!(block_on(sched.tick()), TickResult::Progress));
        assert_eq!(out(&sched, "n"), Some(iteration));
    }
}

#[test]
fn a_snapshot_restores_onto_a_fresh_scheduler() {
    let graph = || {
        builder()
            .add_node("n", inc())
            .connect_feedback("loop", PortRef::new("n", "out"), PortRef::new("n", "in"))
            .build()
    };
    let Loaded { mut sched, .. } = load(graph(), Perf, None);
    for _ in 0..3 {
        let _ = block_on(sched.tick());
    }
    let snapshot = sched.snapshot();
    assert_eq!(snapshot.outputs[&NodeId::from("n")][&"out".into()], "3");

    let Loaded {
        sched: mut restored,
        rebuilds,
    } = load(graph(), Perf, None);
    restored.restore(&snapshot).expect("the same graph");
    assert_eq!(out(&restored, "n"), Some(3));
    assert_eq!(
        restored.node_state(&"n".into()).unwrap().phase(),
        NodePhase::Pending
    );
    let _ = block_on(restored.tick());
    assert_eq!(out(&restored, "n"), Some(4), "the loop carries on");
    assert_eq!(
        rebuilds.load(Ordering::SeqCst),
        1,
        "a restore drops every island"
    );

    let other = builder().add_node("m", inc()).build();
    let Loaded {
        sched: mut other, ..
    } = load(other, Perf, None);
    assert!(matches!(
        other.restore(&snapshot),
        Err(RuntimeError::SnapshotMismatch { .. })
    ));
}

#[test]
fn a_fault_stops_its_island_and_a_rerun_rebuilds_it() {
    let graph = builder()
        .add_node("a", inc())
        .add_node("b", inc())
        .connect("c", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .build();
    let fail = Arc::new(AtomicBool::new(true));
    let Loaded {
        mut sched,
        rebuilds,
    } = load(graph, Perf, Some(("a", fail.clone())));
    settle(&mut sched);
    let faults = sched.take_faults();
    assert!(
        matches!(faults.as_slice(), [report]
            if report.culprit == Some("a".into())
                && matches!(&report.fault, NodeFault::WasmTrap { message } if message == "boom")),
        "{faults:?}"
    );
    let phase = |sched: &Scheduler<Perf, Closures>, node: &str| {
        sched.node_state(&node.into()).unwrap().phase()
    };
    assert_eq!(phase(&sched, "a"), NodePhase::Faulted);
    assert_eq!(out(&sched, "b"), Some(1), "`b` ran on no input");

    fail.store(false, Ordering::SeqCst);
    sched.rerun(&"a".into()).unwrap();
    settle(&mut sched);
    assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
    assert_eq!(phase(&sched, "a"), NodePhase::Idle);
    assert_eq!(out(&sched, "b"), Some(2));
}

#[test]
fn ports_and_limits_are_checked() {
    let graph = builder().add_node("n", inc()).build();
    let Loaded { mut sched, .. } = load(graph, Perf, None);
    assert!(matches!(
        sched.inject(&"n".into(), &"nope".into(), 1),
        Err(RuntimeError::NotAValuePort { .. })
    ));
    assert!(matches!(
        sched.inject(&"x".into(), &"in".into(), 1),
        Err(RuntimeError::UnknownNode { .. })
    ));
    assert!(matches!(
        sched.inject_wave(&"n".into(), &"in".into(), "many"),
        Err(RuntimeError::ValueType { .. })
    ));
    assert!(matches!(
        sched.set_max_steps_per_tick(0),
        Err(RuntimeError::InvalidConfig { .. })
    ));
    assert_eq!(sched.max_steps_per_tick(), 100);
}

#[test]
fn a_run_shape_names_only_contract_inputs() {
    let compiled = builder()
        .add_node("n", inc())
        .build()
        .compile(&[contract("inc")])
        .expect("compiles");
    let shape = RunShape {
        inputs: Some(vec!["extra".into(), "in".into()]),
        has_result: true,
    };
    let islands = compiled
        .islands()
        .iter()
        .map(|members| ((), vec![shape.clone(); members.len()]))
        .collect();
    let executor = Closures {
        behaviour: Arc::new(HashMap::new()),
        ports: HashSet::new(),
        rebuilds: Arc::new(AtomicUsize::new(0)),
    };
    assert!(matches!(
        Scheduler::new(compiled, Perf, executor, islands, 100),
        Err(RuntimeError::InvalidConfig { .. })
    ));
}
