//! Compiled graphs, run: on the scheduler the hosts share, with each node a function
//! whose answer is known. What must run, how often, and what every output then holds
//! is worked out here, apart from the scheduler, and compared after each settling.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use arbitrary::Unstructured;
use futures::executor::block_on;
use witgraph_host_plane::{
    BoxFuture, CallFuture, Canonical, Executor, MemberPlan, OptionPayload, Outcome, Perf, RunShape,
    Scheduler, TickResult, boxed,
};
use witgraph_ir::{CompiledGraph, Graph, NodeId, PortDirection, PortKind, PortRef};

use crate::fail;

type Counts = Arc<Mutex<HashMap<NodeId, usize>>>;

/// What an output holds, apart from its inputs: every port's differs.
fn salt(node: &str, port: &str) -> u32 {
    node.bytes()
        .chain(port.bytes())
        .fold(0x811c_9dc5, |hash: u32, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        })
}

/// Nodes as functions: each output is the sum of what its node's latched inputs hold
/// and its salt. With `constant`, the salt alone, so that a feedback loop settles.
struct Functions {
    /// Per node: which fields of its `inputs` count, and its outputs.
    nodes: HashMap<NodeId, (Vec<bool>, Vec<String>)>,
    ports: HashSet<(PortRef, PortDirection)>,
    runs: Counts,
    constant: bool,
}

impl Executor for Functions {
    type Value = u32;
    type Type = ();

    fn call(&self, member: Arc<MemberPlan>, args: Vec<Option<u32>>) -> CallFuture<u32> {
        *self
            .runs
            .lock()
            .unwrap()
            .entry(member.node.clone())
            .or_default() += 1;
        let (counted, outputs) = &self.nodes[&member.node];
        let sum = if self.constant {
            0
        } else {
            args.iter()
                .zip(counted)
                .filter(|(_, counted)| **counted)
                .filter_map(|(arg, _)| *arg)
                .fold(0u32, u32::wrapping_add)
        };
        let result = outputs
            .iter()
            .map(|port| {
                (
                    port.clone(),
                    sum.wrapping_add(salt(member.node.as_str(), port)),
                )
            })
            .collect();
        boxed(async move { Ok(result) })
    }

    fn rebuild(&self) -> BoxFuture<Outcome> {
        boxed(async { Ok(()) })
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

/// Which nodes have run once nothing more can, by the rule that a node, or a group as
/// one, starts once all its required inputs have a value: injected, or written by a
/// node that has run, over feedback or not.
fn must_run(compiled: &CompiledGraph, injected: &BTreeMap<PortRef, u32>) -> HashSet<NodeId> {
    let graph = compiled.graph();
    let mut unit: HashMap<&NodeId, usize> = HashMap::new();
    for (at, node) in graph.nodes.iter().enumerate() {
        unit.insert(&node.id, at);
    }
    for (group, members) in compiled.stream_groups().iter().enumerate() {
        for member in members {
            unit.insert(member, graph.nodes.len() + group);
        }
    }
    let mut ran: HashSet<NodeId> = HashSet::new();
    loop {
        let mut started: Vec<NodeId> = Vec::new();
        let units: HashSet<usize> = unit.values().copied().collect();
        for candidate in units {
            let members: Vec<&NodeId> = unit
                .iter()
                .filter(|(_, u)| **u == candidate)
                .map(|(node, _)| *node)
                .collect();
            if members.iter().all(|node| ran.contains(*node)) {
                continue;
            }
            let ready = members.iter().all(|node| {
                let contract = compiled.contract_for(node).expect("a compiled node");
                contract
                    .inputs
                    .iter()
                    .filter(|p| !p.is_optional())
                    .all(|input| {
                        let port = PortRef::new((*node).clone(), input.name.clone());
                        injected.contains_key(&port)
                            || graph.connections.iter().any(|c| {
                                c.to == port
                                    && (matches!(input.kind(), PortKind::Stream | PortKind::Future)
                                        || ran.contains(&c.from.node)
                                    // What its group writes does not hold it up.
                                    || (unit.get(&c.from.node) == Some(&candidate) && !c.feedback))
                            })
                    })
            });
            if ready {
                started.extend(members.into_iter().cloned());
            }
        }
        if started.is_empty() {
            return ran;
        }
        ran.extend(started);
    }
}

/// What each Value output holds, in a graph with no feedback, where all nodes run.
fn outputs_of(
    compiled: &CompiledGraph,
    injected: &BTreeMap<PortRef, u32>,
) -> BTreeMap<PortRef, u32> {
    let graph = compiled.graph();
    let mut held: BTreeMap<PortRef, u32> = BTreeMap::new();
    for node in compiled.topological_order() {
        let contract = compiled.contract_for(node).expect("a compiled node");
        let mut sum = 0u32;
        for input in &contract.inputs {
            if !matches!(input.kind(), PortKind::Value | PortKind::Handle) {
                continue;
            }
            let port = PortRef::new(node.clone(), input.name.clone());
            let written = graph.connections.iter().find(|c| c.to == port);
            let value = written
                .and_then(|c| held.get(&c.from).copied())
                .or_else(|| injected.get(&port).copied());
            sum = sum.wrapping_add(value.unwrap_or(0));
        }
        for output in &contract.outputs {
            held.insert(
                PortRef::new(node.clone(), output.name.clone()),
                sum.wrapping_add(salt(node.as_str(), output.name.as_str())),
            );
        }
    }
    held
}

/// Runs `compiled`, changing inputs as `u` says, and compares it all with the above.
pub fn run(graph: &Graph, compiled: CompiledGraph, u: &mut Unstructured<'_>) {
    let has_feedback = graph.connections.iter().any(|c| c.feedback);
    let mut nodes = HashMap::new();
    let mut ports = HashSet::new();
    let mut shapes = HashMap::new();
    for node in compiled.nodes() {
        let id = node.node.id.clone();
        let contract = node.contract;
        let counted = contract
            .inputs
            .iter()
            .map(|p| matches!(p.kind(), PortKind::Value | PortKind::Handle))
            .collect();
        let outputs = contract
            .outputs
            .iter()
            .map(|p| p.name.to_string())
            .collect();
        nodes.insert(id.clone(), (counted, outputs));
        for port in &contract.inputs {
            if port.kind() == PortKind::Value {
                ports.insert((
                    PortRef::new(id.clone(), port.name.clone()),
                    PortDirection::Input,
                ));
            }
        }
        for port in &contract.outputs {
            if port.kind() == PortKind::Value {
                ports.insert((
                    PortRef::new(id.clone(), port.name.clone()),
                    PortDirection::Output,
                ));
            }
        }
        shapes.insert(
            id,
            RunShape {
                inputs: (!contract.inputs.is_empty())
                    .then(|| contract.inputs.iter().map(|p| p.name.to_string()).collect()),
                has_result: !contract.outputs.is_empty(),
            },
        );
    }
    let view = compiled.clone();
    let runs: Counts = Arc::default();
    let executor = Functions {
        nodes,
        ports,
        runs: runs.clone(),
        constant: has_feedback,
    };
    let mut sched = match Scheduler::new(compiled, Perf, executor, &shapes, 64) {
        Ok(sched) => sched,
        Err(error) => fail("a compiled graph does not load", graph, error),
    };

    let mut injected: BTreeMap<PortRef, u32> = BTreeMap::new();
    settle(graph, &view, &mut sched, &runs, &injected, true);

    // Inputs a host may write: Value inputs no plain connection writes.
    let writable: Vec<PortRef> = view
        .nodes()
        .flat_map(|node| {
            node.contract
                .inputs
                .iter()
                .filter(|p| p.kind() == PortKind::Value)
                .map(|p| PortRef::new(node.node.id.clone(), p.name.clone()))
                .collect::<Vec<_>>()
        })
        .filter(|port| {
            !graph
                .connections
                .iter()
                .any(|c| c.to == *port && !c.feedback)
        })
        .collect();
    if writable.is_empty() {
        return;
    }
    for _ in 0..4 {
        let Ok(port) = u.choose(&writable) else {
            return;
        };
        let Ok(value) = u.int_in_range(0..=3u32) else {
            return;
        };
        let same = injected.get(port) == Some(&value);
        if let Err(error) = sched.inject(&port.node, &port.port, value) {
            fail("a host write was refused", graph, (port, error));
        }
        injected.insert(port.clone(), value);
        settle(graph, &view, &mut sched, &runs, &injected, same);
    }
}

fn settle(
    graph: &Graph,
    compiled: &CompiledGraph,
    sched: &mut Scheduler<Perf, Functions>,
    runs: &Counts,
    injected: &BTreeMap<PortRef, u32>,
    first_or_same: bool,
) {
    let before: HashMap<NodeId, usize> = runs.lock().unwrap().clone();
    let limit = graph.nodes.len() * 8 + 32;
    let mut settled = false;
    for _ in 0..limit {
        match block_on(sched.tick()) {
            TickResult::Idle => {
                settled = true;
                break;
            }
            TickResult::Progress | TickResult::StepLimitReached => {}
            other => fail("a tick went wrong", graph, format!("{other:?}")),
        }
    }
    if !settled {
        fail("never settled", graph, limit);
    }

    let ran: HashSet<NodeId> = runs.lock().unwrap().keys().cloned().collect();
    let expected = must_run(compiled, injected);
    if ran != expected {
        let mut missing: Vec<_> = expected.difference(&ran).map(ToString::to_string).collect();
        let mut extra: Vec<_> = ran.difference(&expected).map(ToString::to_string).collect();
        missing.sort();
        extra.sort();
        fail("the wrong nodes ran", graph, (missing, extra, injected));
    }

    if graph.connections.iter().any(|c| c.feedback) {
        return;
    }
    let expected = outputs_of(compiled, injected);
    for node in compiled.nodes() {
        for output in node
            .contract
            .outputs
            .iter()
            .filter(|p| p.kind() == PortKind::Value)
        {
            let port = PortRef::new(node.node.id.clone(), output.name.clone());
            let found = sched.read_output(&port.node, &port.port);
            let found = match found {
                Ok(found) => found,
                Err(error) => fail("reading an output failed", graph, (&port, error)),
            };
            if found != expected.get(&port).copied() {
                fail(
                    "an output holds the wrong value",
                    graph,
                    (&port, found, injected),
                );
            }
        }
    }

    // Everything ran once, in order, or not at all: no glitches.
    let now: HashMap<NodeId, usize> = runs.lock().unwrap().clone();
    let more = |node: &NodeId| {
        now.get(node).copied().unwrap_or(0) - before.get(node).copied().unwrap_or(0)
    };
    for node in &graph.nodes {
        if more(&node.id) > 1 {
            let grouped = compiled.group_of(&node.id).is_some();
            fail(
                "a node ran twice for one change",
                graph,
                (&node.id, more(&node.id), grouped),
            );
        }
        if first_or_same && !before.is_empty() && more(&node.id) > 0 {
            fail("a write of the same value ran a node", graph, &node.id);
        }
    }
}
