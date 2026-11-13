//! Random graphs of real components: composed with their generated hub and run on
//! wasmtime, with every output compared with what these components compute, worked out
//! here, after each host write and across a snapshot and restore.
//!
//! A failure names its seed. `WITGRAPH_REAL_CASES` runs more of them, and
//! `WITGRAPH_REAL_SEED` just the one.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use test_components::{
    FUTURE_READER, FUTURE_WRITER, Guest, MAYBE, RELAY, STREAM_CONSUMER, STREAM_PRODUCER,
    STREAM_RELAY,
};
use witgraph_ir::{ComponentContract, ContractId, Graph, PortRef, ResourceClaim};
use witgraph_runtime::{Perf, RunningGraph, RuntimeConfig, RuntimeGraph, TickResult, Val};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491_4F6C_DD1D)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n.max(1) as u64).unwrap()
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Relay,
    Maybe,
    Producer,
    StreamRelay,
    Consumer,
    Writer,
    Reader,
}

const GUESTS: [(Kind, Guest); 7] = [
    (Kind::Relay, RELAY),
    (Kind::Maybe, MAYBE),
    (Kind::Producer, STREAM_PRODUCER),
    (Kind::StreamRelay, STREAM_RELAY),
    (Kind::Consumer, STREAM_CONSUMER),
    (Kind::Writer, FUTURE_WRITER),
    (Kind::Reader, FUTURE_READER),
];

fn contract(guest: Guest) -> ComponentContract {
    guest.contract()
}

struct Kit {
    contracts: Vec<ComponentContract>,
    wasm: test_components::Kit,
    refs: HashMap<&'static str, ContractId>,
}

fn kit() -> Kit {
    let mut kit = Kit {
        contracts: vec![],
        wasm: test_components::Kit::default(),
        refs: HashMap::new(),
    };
    for (kind, guest) in GUESTS {
        let contract = contract(guest);
        kit.wasm.add(guest);
        kit.refs.insert(name_of(kind), contract.id.clone());
        kit.contracts.push(contract);
    }
    kit
}

fn name_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Relay => "relay",
        Kind::Maybe => "maybe",
        Kind::Producer => "producer",
        Kind::StreamRelay => "stream-relay",
        Kind::Consumer => "consumer",
        Kind::Writer => "writer",
        Kind::Reader => "reader",
    }
}

/// Where a `u32` can come from, and where a stream or a future is waiting to be read.
#[derive(Default)]
struct Plan {
    nodes: Vec<(String, Kind)>,
    connections: Vec<(PortRef, PortRef)>,
    /// `u32` outputs, whatever they hold, and `option<u32>` ones.
    numbers: Vec<PortRef>,
    options: Vec<PortRef>,
    /// Streams and futures yet to be read: each goes to one reader.
    streams: Vec<PortRef>,
    futures: Vec<PortRef>,
    /// What the host writes at the start: every value of a producer's `burst-size`, as
    /// a stream with none is endless.
    writes: Vec<(PortRef, u32)>,
    /// A `maybe` reading its own output back, which holds what it is written.
    loops: Vec<String>,
    /// Fractions of the one resource, by node.
    claims: Vec<(String, f64)>,
}

/// A valid graph, grown forward. Each node reads what is there already.
fn plan(rng: &mut Rng) -> Plan {
    let mut plan = Plan::default();
    let count = 1 + rng.below(9);
    for at in 0..count {
        let id = format!("n{at}");
        let mut options: Vec<Kind> = vec![Kind::Producer, Kind::Writer, Kind::Maybe];
        if !plan.numbers.is_empty() {
            options.extend([Kind::Relay; 3]);
        }
        if !plan.streams.is_empty() {
            options.extend([Kind::Consumer, Kind::Consumer, Kind::StreamRelay]);
        }
        if !plan.futures.is_empty() {
            options.extend([Kind::Reader; 2]);
        }
        let kind = *rng.pick(&options);
        let to = |port: &str| PortRef::new(id.clone(), port);
        let number = |rng: &mut Rng, plan: &Plan| {
            (!plan.numbers.is_empty()).then(|| rng.pick(&plan.numbers).clone())
        };
        match kind {
            Kind::Relay => {
                let from = number(rng, &plan).expect("only with a number to read");
                plan.connections.push((from, to("in")));
                if rng.chance(2) {
                    if !plan.options.is_empty() && rng.chance(2) {
                        let from = rng.pick(&plan.options).clone();
                        plan.connections.push((from, to("add")));
                    } else if let Some(from) = number(rng, &plan) {
                        plan.connections.push((from, to("add")));
                    }
                } else if rng.chance(2) {
                    let value = u32::try_from(rng.below(5)).unwrap();
                    plan.writes.push((to("add"), value));
                }
                plan.numbers.push(to("out"));
            }
            Kind::Maybe => {
                if rng.chance(4) {
                    plan.loops.push(id.clone());
                    if rng.chance(2) {
                        plan.writes
                            .push((to("x"), u32::try_from(rng.below(5)).unwrap()));
                    }
                } else if rng.chance(2) {
                    if let Some(from) = number(rng, &plan) {
                        plan.connections.push((from, to("x")));
                    }
                } else if rng.chance(2) {
                    let value = u32::try_from(rng.below(5)).unwrap();
                    plan.writes.push((to("x"), value));
                }
                plan.options.push(to("maybe"));
            }
            Kind::Producer => {
                let burst = u32::try_from(rng.below(6)).unwrap();
                plan.writes.push((to("burst-size"), burst));
                plan.streams.push(to("items"));
                plan.numbers.push(to("limit"));
            }
            Kind::StreamRelay => {
                let at = rng.below(plan.streams.len());
                plan.connections
                    .push((plan.streams.remove(at), to("items")));
                plan.streams.push(to("items"));
            }
            Kind::Consumer => {
                let at = rng.below(plan.streams.len());
                plan.connections
                    .push((plan.streams.remove(at), to("items")));
                if rng.chance(3) {
                    if let Some(from) = number(rng, &plan) {
                        plan.connections.push((from, to("take")));
                    }
                } else if rng.chance(3) {
                    let value = u32::try_from(rng.below(4)).unwrap();
                    plan.writes.push((to("take"), value));
                }
                if rng.chance(4) {
                    let value = u32::try_from(rng.below(3)).unwrap();
                    plan.writes.push((to("delay"), value));
                }
                plan.numbers.push(to("total"));
                plan.numbers.push(to("count"));
            }
            Kind::Writer => {
                plan.futures.push(to("done"));
            }
            Kind::Reader => {
                let at = rng.below(plan.futures.len());
                plan.connections.push((plan.futures.remove(at), to("data")));
                plan.numbers.push(to("value"));
            }
        }
        if rng.chance(2) {
            plan.claims
                .push((id.clone(), *rng.pick(&[0.2, 0.3, 0.4, 0.6])));
        }
        plan.nodes.push((id, kind));
    }
    // Endless or never read, a stream or a future keeps its group waiting on the rest.
    for port in std::mem::take(&mut plan.streams) {
        let id = format!("d{}", plan.nodes.len());
        plan.connections
            .push((port, PortRef::new(id.clone(), "items")));
        plan.numbers.push(PortRef::new(id.clone(), "total"));
        plan.nodes.push((id, Kind::Consumer));
    }
    for port in std::mem::take(&mut plan.futures) {
        let id = format!("d{}", plan.nodes.len());
        plan.connections
            .push((port, PortRef::new(id.clone(), "data")));
        plan.nodes.push((id, Kind::Reader));
    }
    plan
}

fn graph_of(plan: &Plan, kit: &Kit) -> Graph {
    let mut builder = kit
        .contracts
        .iter()
        .fold(Graph::builder("real"), |builder, c| {
            builder.add_component(c)
        });
    for (id, kind) in &plan.nodes {
        builder = builder.add_node(id.as_str(), kit.refs[name_of(*kind)].clone());
    }
    for (id, amount) in &plan.claims {
        let claim = ResourceClaim::new(*amount).unwrap();
        builder = builder.set_resource(id.as_str(), "gpu", claim).unwrap();
    }
    for (at, (from, to)) in plan.connections.iter().enumerate() {
        builder = builder.connect(format!("c{at}"), from.clone(), to.clone());
    }
    for id in &plan.loops {
        builder = builder.connect_feedback(
            format!("{id}-loop"),
            PortRef::new(id.as_str(), "maybe"),
            PortRef::new(id.as_str(), "x"),
        );
    }
    builder.build()
}

/// What each output holds. A stream has what its producer wrote, from `burst` down its
/// relays; every node is in order, as the plan grows forward.
fn evaluate(plan: &Plan, writes: &BTreeMap<PortRef, u32>) -> BTreeMap<PortRef, Option<u32>> {
    let mut held: BTreeMap<PortRef, Option<u32>> = BTreeMap::new();
    let mut streams: BTreeMap<PortRef, u32> = BTreeMap::new();
    for (id, kind) in &plan.nodes {
        let port = |name: &str| PortRef::new(id.clone(), name);
        let source = |name: &str| {
            let input = port(name);
            plan.connections
                .iter()
                .find(|(_, to)| *to == input)
                .map(|(from, _)| from.clone())
        };
        let read = |name: &str, held: &BTreeMap<PortRef, Option<u32>>| match source(name) {
            Some(from) => held.get(&from).copied().flatten(),
            None => writes.get(&port(name)).copied(),
        };
        match kind {
            Kind::Relay => {
                let sum = read("in", &held)
                    .unwrap_or(0)
                    .wrapping_add(read("add", &held).unwrap_or(0));
                held.insert(port("out"), Some(sum));
            }
            Kind::Maybe => {
                held.insert(port("maybe"), read("x", &held));
            }
            Kind::Producer => {
                let burst = writes.get(&port("burst-size")).copied().unwrap_or(0);
                held.insert(port("limit"), Some(burst));
                streams.insert(port("items"), burst);
            }
            Kind::StreamRelay => {
                let from = source("items").expect("a relay reads a stream");
                streams.insert(port("items"), streams[&from]);
            }
            Kind::Consumer => {
                let from = source("items").expect("a consumer reads a stream");
                let burst = streams[&from];
                let count = read("take", &held).map_or(burst, |take| take.min(burst));
                let total = (0..count).fold(0u32, u32::wrapping_add);
                held.insert(port("total"), Some(total));
                held.insert(port("count"), Some(count));
            }
            Kind::Writer => {}
            Kind::Reader => {
                held.insert(port("value"), Some(7));
            }
        }
    }
    held
}

/// What each output reads as, as WAVE text.
fn expected_waves(plan: &Plan, writes: &BTreeMap<PortRef, u32>) -> BTreeMap<PortRef, String> {
    evaluate(plan, writes)
        .into_iter()
        .map(|(port, value)| {
            let wave = match (port.port.as_str(), value) {
                ("maybe", Some(value)) => format!("some({value})"),
                ("maybe", None) => "none".to_string(),
                (_, value) => value.expect("only a `maybe` holds none").to_string(),
            };
            (port, wave)
        })
        .collect()
}

/// A case, as steps either host can follow.
enum Step {
    Write(PortRef, u32),
    Settle(BTreeMap<PortRef, String>),
    Snapshot,
    Restore,
}

fn script(plan: &Plan, rng: &mut Rng) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut writes: BTreeMap<PortRef, u32> = BTreeMap::new();
    for (port, value) in &plan.writes {
        steps.push(Step::Write(port.clone(), *value));
        writes.insert(port.clone(), *value);
    }
    steps.push(Step::Settle(expected_waves(plan, &writes)));
    let writable: Vec<PortRef> = plan.writes.iter().map(|(port, _)| port.clone()).collect();
    if writable.is_empty() {
        return steps;
    }
    steps.push(Step::Snapshot);
    let at_snapshot = writes.clone();
    for _ in 0..3 {
        let port = rng.pick(&writable).clone();
        let value = u32::try_from(rng.below(6)).unwrap();
        steps.push(Step::Write(port.clone(), value));
        writes.insert(port, value);
        steps.push(Step::Settle(expected_waves(plan, &writes)));
    }
    steps.push(Step::Restore);
    steps.push(Step::Settle(expected_waves(plan, &at_snapshot)));
    steps
}

/// The same case as a file `test-fixtures/scenarios` could hold.
fn scenario(seed: u64, plan: &Plan, steps: &[Step]) -> serde_json::Value {
    let guest = |kind: Kind| {
        GUESTS
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(k, _)| match k {
                Kind::Relay => "relay",
                Kind::Maybe => "maybe",
                Kind::Producer => "stream-producer",
                Kind::StreamRelay => "stream-relay",
                Kind::Consumer => "stream-consumer",
                Kind::Writer => "future-writer",
                Kind::Reader => "future-reader",
            })
    };
    let nodes: Vec<serde_json::Value> = plan
        .nodes
        .iter()
        .map(|(id, kind)| {
            let mut node = json!({ "id": id, "guest": guest(*kind) });
            if let Some((_, amount)) = plan.claims.iter().find(|(claimed, _)| claimed == id) {
                node["resources"] = json!({ "gpu": amount });
            }
            node
        })
        .collect();
    let mut connections: Vec<serde_json::Value> = plan
        .connections
        .iter()
        .enumerate()
        .map(|(at, (from, to))| {
            json!({ "id": format!("c{at}"), "from": from.to_string(), "to": to.to_string() })
        })
        .collect();
    for id in &plan.loops {
        connections.push(json!({
            "id": format!("{id}-loop"),
            "from": format!("{id}.maybe"),
            "to": format!("{id}.x"),
            "feedback": true,
        }));
    }
    let mut out = Vec::new();
    for step in steps {
        match step {
            Step::Write(port, value) => out.push(json!({
                "do": "inject",
                "at": port.to_string(),
                "wave": value.to_string(),
            })),
            Step::Settle(expected) => {
                out.push(json!({ "do": "settle" }));
                for (port, wave) in expected {
                    out.push(json!({ "do": "output", "at": port.to_string(), "expect": wave }));
                }
            }
            Step::Snapshot => out.push(json!({ "do": "snapshot", "expect": {} })),
            Step::Restore => out.push(json!({ "do": "restore" })),
        }
    }
    json!({
        "description": format!("Generated from seed {seed} by `fuzz/tests/real.rs`."),
        "nodes": nodes,
        "connections": connections,
        "steps": out,
    })
}

async fn settle(rt: &mut RuntimeGraph<Perf>, seed: u64, json: &str) {
    for _ in 0..200 {
        let result = tokio::time::timeout(Duration::from_secs(30), rt.tick()).await;
        match result {
            Err(_) => panic!("seed {seed}: a tick hung\n{json}"),
            Ok(TickResult::Idle) => return,
            Ok(TickResult::Progress | TickResult::StepLimitReached) => {}
            Ok(other) => panic!("seed {seed}: a tick went wrong: {other:?}\n{json}"),
        }
    }
    panic!("seed {seed}: never settled\n{json}");
}

async fn case(seed: u64, kit: &Kit, config: RuntimeConfig) {
    let mut rng = Rng::new(seed);
    let plan = plan(&mut rng);
    let graph = graph_of(&plan, kit);
    let json = serde_json::to_string(&graph).unwrap();
    let compiled = match graph.compile(&kit.contracts) {
        Ok(compiled) => compiled,
        Err(failure) => panic!("seed {seed}: does not compile: {failure}\n{json}"),
    };
    let mut rt = RuntimeGraph::load(compiled, &kit.wasm.artifacts(), config, Perf)
        .await
        .unwrap_or_else(|error| panic!("seed {seed}: does not load: {error}\n{json}"));

    let steps = script(&plan, &mut rng);
    if let Some(directory) = std::env::var_os("WITGRAPH_REAL_EXPORT") {
        let text = serde_json::to_string_pretty(&scenario(seed, &plan, &steps)).unwrap();
        std::fs::write(
            Path::new(&directory).join(format!("seed-{seed}.json")),
            text + "\n",
        )
        .unwrap();
    }
    let mut snapshot = None;
    for (at, step) in steps.iter().enumerate() {
        match step {
            Step::Write(port, value) => {
                rt.inject(&port.node, &port.port, Val::U32(*value))
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: writing `{port}`: {e}\n{json}"));
            }
            Step::Settle(expected) => {
                settle(&mut rt, seed, &json).await;
                for (port, wanted) in expected {
                    let found = rt.read_output_wave(&port.node, &port.port).await.unwrap();
                    assert_eq!(
                        found.as_deref(),
                        Some(wanted.as_str()),
                        "seed {seed}: `{port}` at step {at}\n{json}"
                    );
                }
            }
            Step::Snapshot => snapshot = Some(rt.snapshot().await.unwrap()),
            Step::Restore => rt.restore(snapshot.as_ref().unwrap()).await.unwrap(),
        }
    }
}

fn env(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn real_graphs_compute_what_they_should() {
    let kit = kit();
    let mut seeds: Vec<u64> = match env("WITGRAPH_REAL_SEED") {
        Some(seed) => vec![seed],
        None => (1..=env("WITGRAPH_REAL_CASES").unwrap_or(25)).collect(),
    };
    seeds.reverse();
    while let Some(seed) = seeds.pop() {
        case(seed, &kit, RuntimeConfig::default()).await;
    }
}
