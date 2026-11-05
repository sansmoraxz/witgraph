//! The scheduler on a plane a test answers by hand: what it asks, and what it does
//! with the answers, in orders a test chooses.

use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use witgraph_ir::{
    ComponentContract, ComponentRef, FeedbackIdx, Graph, InputIdx, NodeId, NodeIdx, PortDef,
    PortRef, Type,
};

use crate::control::SchedulerCore;
use crate::executor::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, Incarnation, Outcome, Returned,
    Serving, boxed,
};
use crate::{NodePhase, Perf, TickResult};

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
    stream_group_with(claims, None)
}

/// The same, with a capacity declared for the resource they claim.
fn stream_group_with(claims: bool, capacity: Option<f64>) -> (SchedulerCore<Scripted>, Scripted) {
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
        vec![PortDef::stream("s", witgraph_ir::Carried::Data(Type::U32))],
    );
    let reader = contract(
        "reader",
        vec![PortDef::stream("s", witgraph_ir::Carried::Data(Type::U32))],
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
    if let Some(capacity) = capacity {
        builder = builder.set_capacity("gpu", witgraph_ir::Capacity::new(capacity).unwrap());
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

/// What does not fit at the default capacity starts beside it at a larger one.
#[test]
fn a_capacity_lets_a_node_start_beside_a_group() {
    let (mut core, plane) = stream_group_with(true, Some(1.2));
    let (p, v) = (index(&core, "p"), index(&core, "v"));
    assert!(poll_tick(&mut core).is_none());
    let started = plane.commands();
    assert!(started.contains(&Command::Run(p)), "{started:?}");
    assert!(started.contains(&Command::Run(v)), "{started:?}");
}

/// A report the dropped tick never got to is handled before anything else.
#[test]
fn what_a_dropped_tick_left_behind_is_handled_first() {
    let (mut core, plane) = stream_group(false);
    let v = index(&core, "v");
    assert!(poll_tick(&mut core).is_none());
    assert!(plane.commands().contains(&Command::Run(v)));
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    let id = NodeId::from("v");
    assert_eq!(
        core_dyn.node_state(&id).unwrap().phase(),
        NodePhase::Running
    );
    plane.returned(v);
    assert!(core_dyn.take_faults().is_empty());
    assert_eq!(core_dyn.node_state(&id).unwrap().phase(), NodePhase::Idle);
}

/// The question of the group's drain and of the whole instance's quiet, as a group's
/// last `run` returning asks them: `(the drain's number, the quiet's number)`.
fn quiet_after_drain(plane: &Scripted) -> (u64, u64) {
    let commands = plane.commands();
    let [
        Command::AwaitDrained { at: drain, .. },
        Command::AwaitQuiet(quiet),
    ] = commands[..]
    else {
        panic!("expected a drain, then quiet, asked after: {commands:?}");
    };
    (drain, quiet)
}

#[test]
fn a_group_can_drain_while_the_instance_is_not_quiet() {
    let (mut core, plane) = stream_group(true);
    let (p, r, v) = (index(&core, "p"), index(&core, "r"), index(&core, "v"));
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(p)]);
    plane.returned(p);
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(r)]);
    plane.returned(r);
    assert!(poll_tick(&mut core).is_none());
    let (drain, _) = quiet_after_drain(&plane);
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    assert_eq!(
        core_dyn.node_state(&"p".into()).unwrap().phase(),
        NodePhase::Running,
        "its writers may be going on"
    );

    // An answer for a drain before this one says nothing of this one.
    plane.report(ControlKind::Drained {
        group: 0,
        at: drain + 1,
    });
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [], "it is still draining");

    // Quiet never came: guest work goes on somewhere, but this group's is over.
    plane.report(ControlKind::Drained {
        group: 0,
        at: drain,
    });
    assert!(poll_tick(&mut core).is_none());
    assert_eq!(plane.commands(), [Command::Run(v)], "it gave up its claim");
    let core_dyn: &mut SchedulerCore<dyn DataPlane> = &mut core;
    assert_eq!(
        core_dyn.node_state(&"p".into()).unwrap().phase(),
        NodePhase::Idle
    );
    assert_eq!(
        core_dyn.node_state(&"r".into()).unwrap().phase(),
        NodePhase::Idle
    );
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
    let (_, at) = quiet_after_drain(&plane);

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
    let (_, at) = quiet_after_drain(&plane);
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
    let (_, at) = quiet_after_drain(&plane);
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
        inputs: vec![PortDef::value("in", Type::U32)],
        outputs: vec![PortDef::value("out", Type::U32)],
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
