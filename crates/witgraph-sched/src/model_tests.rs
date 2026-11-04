//! The scheduler under random events, in any order a host could produce them: runs
//! returning, latches answered, host writes, reruns, shutdowns, faults, reports
//! from an instance long gone, and ticks dropped at any point. The tests elsewhere
//! pin chosen interleavings; this holds every interleaving a seed comes up with to
//! what must always be so:
//!
//! - no node has two `run`s in flight;
//! - nothing starts while a command is unanswered;
//! - a node starts with its required inputs present;
//! - what runs at once fits its resource claims;
//! - once everything is answered the graph settles, and can still run.
//!
//! A failure names its seed. `WITGRAPH_MODEL_SEEDS` runs more of them.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use witgraph_ir::{
    ComponentContract, ComponentRef, FeedbackIdx, Graph, InputIdx, NodeIdx, PortDef, PortRef,
    ResourceClaim, Type,
};

use crate::control::SchedulerCore;
use crate::executor::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, Incarnation, Latched, Outcome,
    Returned, Serving, boxed,
};
use crate::{NodeFault, Perf, TickResult};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())])
    }
}

/// One instance served.
struct Instance {
    incarnation: Incarnation,
    commands: UnboundedReceiver<Command>,
    events: UnboundedSender<ControlEvent>,
    fail: Option<oneshot::Sender<()>>,
}

/// A plane that does nothing until told to: the test answers for it.
#[derive(Clone, Default)]
struct Plane {
    instances: Arc<Mutex<Vec<Instance>>>,
}

impl DataPlane for Plane {
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome> {
        let (fail, failed) = oneshot::channel();
        self.instances.lock().unwrap().push(Instance {
            incarnation: serving.incarnation,
            commands: serving.commands,
            events: serving.events,
            fail: Some(fail),
        });
        boxed(async move {
            match failed.await {
                Ok(()) => Err((
                    NodeFault::WasmTrap {
                        message: "the model faulted".into(),
                    },
                    None,
                )),
                // The test let go of it: it serves on, as far as the scheduler knows.
                Err(_) => futures::future::pending().await,
            }
        })
    }

    fn stopped(&self) {}
}

impl Plane {
    fn report(&self, kind: ControlKind) {
        let instances = self.instances.lock().unwrap();
        let instance = instances.last().unwrap();
        let _ = instance.events.unbounded_send(ControlEvent {
            incarnation: instance.incarnation,
            kind,
        });
    }

    /// A report from an instance before the live one, if there was one.
    fn report_late(&self, kind: ControlKind) {
        let instances = self.instances.lock().unwrap();
        if let [.., old, _] = instances.as_slice() {
            let _ = old.events.unbounded_send(ControlEvent {
                incarnation: old.incarnation,
                kind,
            });
        }
    }

    fn fail(&self) {
        let mut instances = self.instances.lock().unwrap();
        if let Some(fail) = instances.last_mut().and_then(|i| i.fail.take()) {
            let _ = fail.send(());
        }
    }

    fn take_commands(&self) -> Vec<Command> {
        let mut instances = self.instances.lock().unwrap();
        let mut taken = Vec::new();
        if let Some(instance) = instances.last_mut() {
            while let Ok(command) = instance.commands.try_recv() {
                taken.push(command);
            }
        }
        taken
    }

    fn count(&self) -> usize {
        self.instances.lock().unwrap().len()
    }
}

fn contract(world: &str, inputs: Vec<PortDef>, outputs: Vec<PortDef>) -> ComponentContract {
    ComponentContract {
        id: ComponentRef {
            content_hash: Some("cc".into()),
            ..(format!("test:nodes/{world}@0.1.0").parse().unwrap())
        },
        inputs,
        outputs,
        capabilities: vec![],
        docs: None,
    }
}

fn value(name: &str) -> PortDef {
    PortDef::value(name, Type::U32)
}

fn stream(name: &str) -> PortDef {
    PortDef::stream(name, witgraph_ir::Carried::Data(Type::U32))
}

/// Every kind of thing a graph holds, in a few nodes:
///
/// - `a -> b` by a required input, and `b` feeding back into `c`'s required one;
/// - `c`'s other input for the host to write;
/// - `p -> q` over a stream, `q` reading `c` too, and `p` and `v` sharing a resource.
fn graph() -> witgraph_ir::CompiledGraph {
    let source = contract("source", vec![value("x").optional()], vec![value("out")]);
    let middle = contract("middle", vec![value("in")], vec![value("out")]);
    let looped = contract(
        "looped",
        vec![value("in"), value("x").optional()],
        vec![value("out")],
    );
    let head = contract("head", vec![], vec![stream("s")]);
    let tail = contract("tail", vec![stream("s"), value("x").optional()], vec![]);
    let contracts = [
        source.clone(),
        middle.clone(),
        looped.clone(),
        head.clone(),
        tail.clone(),
    ];
    let half = || ResourceClaim::new(0.6).unwrap();
    Graph::builder("model")
        .add_component(&source)
        .add_component(&middle)
        .add_component(&looped)
        .add_component(&head)
        .add_component(&tail)
        .add_node("a", source.id.clone())
        .add_node("b", middle.id.clone())
        .add_node("c", looped.id.clone())
        .add_node("p", head.id.clone())
        .add_node("q", tail.id.clone())
        .add_node("v", source.id.clone())
        .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
        .connect_feedback("bc", PortRef::new("b", "out"), PortRef::new("c", "in"))
        .connect("pq", PortRef::new("p", "s"), PortRef::new("q", "s"))
        .connect("cq", PortRef::new("c", "out"), PortRef::new("q", "x"))
        .set_resource("p", "gpu", half())
        .unwrap()
        .set_resource("v", "gpu", half())
        .unwrap()
        .build()
        .compile(&contracts)
        .unwrap()
}

/// What the plane's lane serves.
#[derive(Debug, Clone, Copy)]
enum Job {
    Latch(FeedbackIdx),
    Write(InputIdx),
    Clear(InputIdx),
    Restore,
}

/// What the plane has been asked and not answered, and what it holds.
#[derive(Default)]
struct Pending {
    runs: Vec<NodeIdx>,
    /// The lane: what is served one at a time, in the order it was asked.
    lane: VecDeque<Job>,
    quiet: Option<u64>,
    /// The inputs with a value.
    present: BTreeSet<InputIdx>,
    /// The inputs the host wrote, written to each new instance.
    written: BTreeSet<InputIdx>,
    /// Feedback buffered, by connection.
    buffered: BTreeSet<FeedbackIdx>,
    /// How many instances it knows of.
    instances: usize,
}

struct Model {
    core: SchedulerCore<Plane>,
    plane: Plane,
    pending: Pending,
    seed: u64,
    log: Vec<String>,
}

impl Model {
    fn new(seed: u64) -> Self {
        let plane = Plane::default();
        let core = SchedulerCore::new(graph(), Arc::new(Perf), 1000, |_, _| plane.clone()).unwrap();
        Self {
            core,
            plane,
            pending: Pending {
                instances: 1,
                ..Pending::default()
            },
            seed,
            log: Vec::new(),
        }
    }

    fn fail(&self, message: &str) -> ! {
        let tail = self.log.len().saturating_sub(40);
        panic!(
            "seed {}: {message}\nlast steps:\n  {}",
            self.seed,
            self.log[tail..].join("\n  ")
        );
    }

    fn erased(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        &mut self.core
    }

    fn node(&self, name: &str) -> NodeIdx {
        self.core.ports().node(&name.into()).unwrap()
    }

    fn required(&self, node: NodeIdx) -> Vec<InputIdx> {
        let (b, c) = (self.node("b"), self.node("c"));
        let input =
            |name: &str, port: &str| self.core.ports().input(&name.into(), &port.into()).unwrap();
        if node == b {
            vec![input("b", "in")]
        } else if node == c {
            vec![input("c", "in")]
        } else {
            Vec::new()
        }
    }

    /// Takes in what the scheduler asked, checking it may ask that now.
    fn observe(&mut self) {
        // An instance that stopped answers nothing more: its server is gone.
        if !self.core.is_live() || self.plane.count() != self.pending.instances {
            self.log.push("  (the instance is gone)".into());
            // What a stopped one was asked goes with it; a new one's is for it.
            if !self.core.is_live() {
                let _ = self.plane.take_commands();
            }
            self.pending = Pending {
                instances: self.plane.count(),
                present: self.pending.written.clone(),
                written: std::mem::take(&mut self.pending.written),
                ..Pending::default()
            };
        }
        for command in self.plane.take_commands() {
            self.log.push(format!("  asked {command:?}"));
            match command {
                Command::Run(node) => {
                    if self.pending.runs.contains(&node) {
                        self.fail(&format!("{node:?} started with a run in flight"));
                    }
                    if !self.pending.lane.is_empty() {
                        self.fail(&format!("{node:?} started with a command unanswered"));
                    }
                    for input in self.required(node) {
                        if !self.pending.present.contains(&input) {
                            self.fail(&format!("{node:?} started with no value at {input:?}"));
                        }
                    }
                    self.pending.runs.push(node);
                    self.check_claims();
                }
                Command::LatchFeedback(connection) => {
                    if !self.pending.buffered.contains(&connection) {
                        self.fail(&format!("latched {connection:?}, with nothing buffered"));
                    }
                    self.pending.lane.push_back(Job::Latch(connection));
                }
                Command::DropFeedback(connection) => {
                    self.pending.buffered.remove(&connection);
                }
                Command::AwaitQuiet(at) => self.pending.quiet = Some(at),
            }
        }
    }

    /// `v` alone, or the group `{p, q}` once either has started, holds the resource.
    fn check_claims(&self) {
        let (v, group) = (self.node("v"), [self.node("p"), self.node("q")]);
        let lone = self.pending.runs.contains(&v);
        let grouped = group.iter().any(|m| self.pending.runs.contains(m));
        if lone && grouped {
            self.fail("`v` and the group run at once, past their claims");
        }
    }

    /// A `run` returns, with what a random `run` wrote.
    fn return_run(&mut self, rng: &mut Rng, changes: bool) {
        if self.pending.runs.is_empty() {
            return;
        }
        let at = rng.below(self.pending.runs.len());
        let node = self.pending.runs.remove(at);
        let mut returned = Returned::default();
        let ports = self.core.ports().clone();
        let input = |n: &str, p: &str| ports.input(&n.into(), &p.into()).unwrap();
        if changes || rng.below(2) == 0 {
            if node == self.node("a") {
                returned.inputs_changed.push(input("b", "in"));
                self.pending.present.insert(input("b", "in"));
            } else if node == self.node("c") {
                returned.inputs_changed.push(input("q", "x"));
                self.pending.present.insert(input("q", "x"));
            }
        }
        if node == self.node("b") && (changes || rng.below(2) == 0) {
            let connection = ports.feedback_index(&"bc".into()).unwrap();
            returned.feedback_buffered.push(connection);
            self.pending.buffered.insert(connection);
        }
        if node == self.node("p") {
            returned.inputs_recorded.push(input("q", "s"));
            self.pending.present.insert(input("q", "s"));
        }
        self.log.push(format!("  returned {node:?}: {returned:?}"));
        self.plane
            .report(ControlKind::RunReturned { node, returned });
    }

    /// Serves the next job of the lane.
    fn serve_lane(&mut self, rng: &mut Rng, change: bool) {
        let Some(job) = self.pending.lane.pop_front() else {
            return;
        };
        self.log.push(format!("  served {job:?}"));
        match job {
            Job::Latch(connection) => {
                self.pending.buffered.remove(&connection);
                let to = self.core.ports().feedback()[connection].to;
                let latched = match (change, rng.below(3)) {
                    (false, _) | (_, 0) if self.pending.present.contains(&to) => Latched::Unchanged,
                    (_, 2) if change => {
                        self.pending.present.remove(&to);
                        Latched::Cleared
                    }
                    _ => {
                        self.pending.present.insert(to);
                        Latched::Changed
                    }
                };
                self.log.push(format!("    {latched:?}"));
                self.plane.report(ControlKind::Latched {
                    connection,
                    latched,
                });
            }
            Job::Write(input) => {
                let changed = change || !self.pending.present.contains(&input);
                self.pending.present.insert(input);
                self.plane.report(ControlKind::HostWrote { input, changed });
            }
            Job::Clear(input) => {
                let cleared = self.pending.present.remove(&input);
                self.plane
                    .report(ControlKind::HostCleared { input, cleared });
            }
            Job::Restore => self.plane.report(ControlKind::Restored),
        }
    }

    fn answer_quiet(&mut self) {
        if !self.pending.runs.is_empty() {
            return;
        }
        if let Some(at) = self.pending.quiet.take() {
            self.log.push(format!("  quiet at {at}"));
            self.plane.report(ControlKind::Quiet(at));
        }
    }

    /// A host write, in either way a host makes one.
    fn host_write(&mut self, rng: &mut Rng) {
        let ports = self.core.ports().clone();
        let names = [("a", "x"), ("c", "in"), ("c", "x"), ("v", "x")];
        let (node, port) = names[rng.below(names.len())];
        let input = ports.input(&node.into(), &port.into()).unwrap();
        let clear = rng.below(4) == 0;
        let live = self.core.is_live();
        self.log
            .push(format!("write {node}.{port}, clear {clear}, live {live}"));
        if clear {
            self.pending.written.remove(&input);
        } else {
            self.pending.written.insert(input);
        }
        if live {
            if clear {
                self.erased().host_clear_started(input);
                self.pending.lane.push_back(Job::Clear(input));
            } else {
                self.erased().host_write_started(input);
                self.pending.lane.push_back(Job::Write(input));
            }
        } else if clear {
            self.erased().host_cleared(input);
        } else {
            self.erased().host_set(input);
        }
    }

    /// One thing happens.
    fn step(&mut self, rng: &mut Rng) {
        self.observe();
        match rng.below(14) {
            0..=2 => {
                self.log.push("tick, dropped".into());
                let _ = self.erased().tick().now_or_never();
            }
            3 => {
                self.log.push("tick, interrupted".into());
                let _ = self
                    .erased()
                    .tick_until(std::future::ready(()))
                    .now_or_never();
            }
            4..=6 => self.return_run(rng, false),
            7 | 9 => self.serve_lane(rng, true),
            8 => self.answer_quiet(),
            10 => self.host_write(rng),
            11 => {
                let names = ["a", "b", "c", "p", "q", "v"];
                let node = rng.pick(&names).unwrap();
                self.log.push(format!("rerun {node}"));
                self.erased().rerun(&node.into()).unwrap();
            }
            12 => match rng.below(6) {
                3 => self.restore(),
                0 => {
                    self.log.push("shutdown".into());
                    self.erased().shutdown();
                }
                1 => {
                    self.log.push("fault".into());
                    self.plane.fail();
                }
                _ => {
                    self.log.push("drain".into());
                    self.erased().drain();
                }
            },
            _ => {
                // Whatever an instance long gone had to say.
                self.log.push("a late report".into());
                let node = self.node("a");
                self.plane.report_late(ControlKind::RunReturned {
                    node,
                    returned: Returned::default(),
                });
                self.plane.report_late(ControlKind::Restored);
            }
        }
        self.observe();
    }

    /// Answers everything, changing nothing more, until a tick finds nothing to do.
    fn settle(&mut self, rng: &mut Rng) {
        for _ in 0..500 {
            self.observe();
            while !self.pending.runs.is_empty() {
                self.return_run(rng, false);
            }
            while !self.pending.lane.is_empty() {
                self.serve_lane(rng, false);
            }
            self.answer_quiet();
            self.log.push("tick, to settle".into());
            let tick = self.erased().tick().now_or_never();
            self.observe();
            if matches!(tick, Some(TickResult::Idle))
                && self.pending.runs.is_empty()
                && self.pending.lane.is_empty()
            {
                return;
            }
        }
        self.fail("never settled, with everything answered");
    }

    /// A node with no input to wait for runs when asked, however the graph got here.
    fn check_alive(&mut self, rng: &mut Rng) {
        let v = self.node("v");
        self.erased().rerun(&"v".into()).unwrap();
        for _ in 0..500 {
            self.log.push("tick, to see `v` run".into());
            let _ = self.erased().tick().now_or_never();
            self.observe();
            if self.pending.runs.contains(&v) {
                return;
            }
            while !self.pending.runs.is_empty() {
                self.return_run(rng, false);
            }
            while !self.pending.lane.is_empty() {
                self.serve_lane(rng, false);
            }
            self.answer_quiet();
        }
        self.fail("`v` was asked to run, and never did");
    }

    /// A restore of what the graph holds now, on a new instance.
    fn restore(&mut self) {
        let snapshot = self.erased().snapshot_base();
        if self.erased().check_snapshot(&snapshot).is_err() {
            self.log.push("restore, refused".into());
            return;
        }
        self.log.push("restore".into());
        let present = std::mem::take(&mut self.pending.present);
        let written = std::mem::take(&mut self.pending.written);
        let host_written = self.erased().host_written_in(&snapshot);
        self.erased().restart_for_restore();
        let presence: Vec<InputIdx> = present.iter().copied().collect();
        self.erased().apply_restore(&presence, &[], &snapshot);
        self.erased().restore_writes_started();
        self.pending = Pending {
            instances: self.plane.count(),
            present,
            written: written
                .into_iter()
                .filter(|input| host_written.contains(*input))
                .collect(),
            lane: VecDeque::from([Job::Restore]),
            ..Pending::default()
        };
    }
}

#[test]
fn every_order_of_events_keeps_the_scheduler_sound() {
    let seeds: u64 = std::env::var("WITGRAPH_MODEL_SEEDS")
        .ok()
        .map_or(400, |seeds| seeds.parse().unwrap());
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut model = Model::new(seed);
        for _ in 0..150 {
            model.step(&mut rng);
        }
        model.settle(&mut rng);
        model.check_alive(&mut rng);
    }
}
