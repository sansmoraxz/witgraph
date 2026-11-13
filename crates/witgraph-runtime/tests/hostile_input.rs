//! Whatever comes in from outside, a bad input is an error, not a panic, and not a
//! stack overflow, which no error handling catches: real components, WIT and graphs,
//! with their bytes changed at random. A browser runs all of this in one module, so
//! one panic here ends every session on the page.
//!
//! A failure names its seed. `WITGRAPH_HOSTILE_CASES` runs more of them, and
//! `WITGRAPH_HOSTILE_SEED` just the one.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use test_components::{
    BUSY_LOOP, CORPUS_RELAY, ECHO, Guest, RELAY, STREAM_PRODUCER, TENSOR_SINK, TENSOR_SOURCE,
};
use witgraph_ir::{ComponentContract, ContractId, Graph};
use witgraph_wit::hub::{self, HubMember};
use witgraph_wit::load::WitSource;

const GUESTS: [Guest; 7] = [
    ECHO,
    RELAY,
    BUSY_LOOP,
    CORPUS_RELAY,
    STREAM_PRODUCER,
    TENSOR_SOURCE,
    TENSOR_SINK,
];

/// How far inputs got, as a test whose inputs all fail at the door proves nothing.
#[derive(Default)]
struct Reached {
    wit_parsed: AtomicUsize,
    wit_lowered: AtomicUsize,
    component_lowered: AtomicUsize,
    composed: AtomicUsize,
    graph_parsed: AtomicUsize,
    graph_compiled: AtomicUsize,
}

static REACHED: LazyLock<Reached> = LazyLock::new(Reached::default);

fn count(stage: &AtomicUsize) {
    stage.fetch_add(1, Ordering::Relaxed);
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
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
}

/// `input`, a few changes away from itself.
fn mutated(rng: &mut Rng, input: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(out.len());
        match rng.below(7) {
            0 => out[at] ^= 1 << rng.below(8),
            1 => out[at] = [0, 1, 0x7F, 0x80, 0xFF][rng.below(5)],
            2 => out.truncate(at),
            3 => {
                // A LEB128 length, claiming far more than there is.
                out.splice(at..at, [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
            }
            4 => {
                let end = (at + 1 + rng.below(32)).min(out.len());
                out.drain(at..end);
            }
            5 => {
                let end = (at + 1 + rng.below(64)).min(out.len());
                let copied = out[at..end].to_vec();
                let to = rng.below(out.len());
                out.splice(to..to, copied);
            }
            _ => {
                let chunk = chunks[rng.below(chunks.len())];
                out.splice(at..at, chunk.iter().copied());
            }
        }
        if out.is_empty() {
            out.push(0);
        }
    }
    out
}

/// Runs `step`: an error is fine, and a panic is what this test is for.
fn no_panic(seed: u64, what: &str, input: &[u8], step: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(step)).is_err() {
        let shown = String::from_utf8_lossy(&input[..input.len().min(400)]);
        panic!("seed {seed}: {what} panicked, on an input starting:\n{shown}");
    }
}

const WIT_CHUNKS: [&[u8]; 16] = [
    b"record ",
    b"variant ",
    b"resource r;",
    b"borrow<",
    b"own<",
    b"list<",
    b"option<",
    b"stream<",
    b"future<",
    b"tuple<",
    b"use ",
    b" include ",
    b"@since(version = 1.0.0) ",
    b"@unstable(feature = x) ",
    b"\xF0\x9F\xA6\x80",
    b"%",
];

const WASM_CHUNKS: [&[u8]; 4] = [
    b"\0asm",
    b"component-type",
    b"component-import",
    b"\x00\x00\x00",
];

const JSON_CHUNKS: [&[u8]; 6] = [b"null", b"[]", b"{}", b"\"\"", b"1e999", b"\"a:b/c@1.0.0\""];

fn wit_case(seed: u64, rng: &mut Rng, source: &str) {
    let wit = mutated(rng, source.as_bytes(), &WIT_CHUNKS);
    let Ok(text) = String::from_utf8(wit.clone()) else {
        return;
    };
    no_panic(seed, "lowering WIT", &wit, || {
        if let Ok(source) = witgraph_wit::load::load_str("hostile.wit", &text) {
            count(&REACHED.wit_parsed);
            if witgraph_wit::lower::lower(&source, &"c".parse().unwrap()).is_ok() {
                count(&REACHED.wit_lowered);
            }
        }
    });
}

fn wasm_case(
    seed: u64,
    rng: &mut Rng,
    guest: Guest,
    wit: &WitSource,
    contract: &ComponentContract,
) {
    let bytes = mutated(rng, guest.wasm, &WASM_CHUNKS);
    no_panic(seed, "reading a component's imports", &[], || {
        let _ = witgraph_wit::verify::Imports::of(&bytes);
    });
    no_panic(seed, "verifying a component", &[], || {
        let _ = witgraph_wit::verify::verify_component(wit, &contract.id.world, &bytes);
    });
}

/// A component that still targets its world goes on to be composed.
fn compose_case(
    seed: u64,
    rng: &mut Rng,
    guest: Guest,
    wit: &WitSource,
    contract: &ComponentContract,
) {
    let bytes = mutated(rng, guest.wasm, &WASM_CHUNKS);
    if witgraph_wit::verify::verify_component(wit, &contract.id.world, &bytes).is_err() {
        return;
    }
    count(&REACHED.component_lowered);
    let Ok(compiled) = Graph::builder("g")
        .add_component(contract)
        .add_node("n", contract.id.clone())
        .build()
        .compile(std::slice::from_ref(contract))
    else {
        return;
    };
    let node = "n".into();
    let members = [HubMember {
        node: &node,
        bytes: &bytes,
        export: "node",
        wit,
        world: &contract.id.world,
        mocked: false,
    }];
    no_panic(seed, "composing a hub", &[], || {
        if hub::compose_graph(&compiled, &members, hub::HubOptions::default()).is_ok() {
            count(&REACHED.composed);
        }
    });
}

/// Every place in `value` that holds something, by path.
fn leaves(value: &Value, at: &mut Vec<usize>, found: &mut Vec<Vec<usize>>) {
    found.push(at.clone());
    let children: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(fields) => fields.values().collect(),
        _ => Vec::new(),
    };
    for (index, child) in children.into_iter().enumerate() {
        at.push(index);
        leaves(child, at, found);
        at.pop();
    }
}

fn at_path<'v>(value: &'v mut Value, path: &[usize]) -> &'v mut Value {
    path.iter().fold(value, |value, index| match value {
        Value::Array(items) => &mut items[*index],
        Value::Object(fields) => fields.values_mut().nth(*index).unwrap(),
        _ => panic!("no such path"),
    })
}

/// A graph with its fields changed: well-formed JSON, of a shape `compile` gets to
/// judge. Names come from the graph, so they refer to what it holds, wrongly.
fn mutated_graph(rng: &mut Rng, base: &Value) -> Value {
    let mut graph = base.clone();
    for _ in 0..1 + rng.below(3) {
        let mut paths = Vec::new();
        leaves(&graph, &mut Vec::new(), &mut paths);
        let path = paths[rng.below(paths.len())].clone();
        let other = paths[rng.below(paths.len())].clone();
        let copied = at_path(&mut graph, &other).clone();
        let target = at_path(&mut graph, &path);
        match (target, rng.below(5)) {
            (Value::Array(items), 0) if !items.is_empty() => {
                let at = rng.below(items.len());
                items.remove(at);
            }
            (Value::Array(items), 1) if !items.is_empty() => {
                let at = rng.below(items.len());
                items.push(items[at].clone());
            }
            (Value::Bool(flag), _) => *flag = !*flag,
            (Value::String(text), 2) => text.clear(),
            (Value::String(text), 3) => *text = ["a", "b", "in", "out", "add"][rng.below(5)].into(),
            (target, _) => *target = copied,
        }
    }
    graph
}

fn graph_case(seed: u64, rng: &mut Rng, base: &str, contracts: &[ComponentContract]) {
    let json = if rng.below(4) == 0 {
        mutated(rng, base.as_bytes(), &JSON_CHUNKS)
    } else {
        let parsed: Value = serde_json::from_str(base).unwrap();
        serde_json::to_vec(&mutated_graph(rng, &parsed)).unwrap()
    };
    no_panic(seed, "compiling a graph", &json, || {
        if let Ok(graph) = serde_json::from_slice::<Graph>(&json) {
            count(&REACHED.graph_parsed);
            if graph.compile(contracts).is_ok() {
                count(&REACHED.graph_compiled);
            }
        }
    });
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let var = |name: &str| std::env::var(name).ok().map(|v| v.parse::<u64>().unwrap());
    let single = var("WITGRAPH_HOSTILE_SEED").is_some();
    let seeds = match var("WITGRAPH_HOSTILE_SEED") {
        Some(seed) => seed..seed + 1,
        None => 0..var("WITGRAPH_HOSTILE_CASES").unwrap_or(1000),
    };
    let contracts: Vec<ComponentContract> = GUESTS.iter().map(|guest| guest.contract()).collect();
    let wits: Vec<WitSource> = GUESTS.iter().map(|guest| guest.wit_source()).collect();
    let sources: Vec<String> = GUESTS
        .iter()
        .map(|guest| std::fs::read_to_string(format!("{}/world.wit", guest.wit)).unwrap())
        .collect();
    let refs: Vec<&ContractId> = contracts.iter().map(|c| &c.id).collect();
    let graph = Graph::builder("g")
        .add_component(&contracts[1])
        .add_node("a", refs[1].clone())
        .add_node("b", refs[1].clone())
        .connect(
            "ab",
            witgraph_ir::PortRef::new("a", "out"),
            witgraph_ir::PortRef::new("b", "in"),
        )
        .connect_feedback(
            "ba",
            witgraph_ir::PortRef::new("b", "out"),
            witgraph_ir::PortRef::new("a", "in"),
        )
        .build();
    let base = serde_json::to_string(&graph).unwrap();
    // The hook would print every panic caught; the failure says which it was.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = catch_unwind(|| {
        for seed in seeds {
            let mut rng = Rng::new(seed);
            let at = rng.below(GUESTS.len());
            let (guest, contract) = (GUESTS[at], &contracts[at]);
            match rng.below(4) {
                0 => wit_case(seed, &mut rng, &sources[at]),
                1 => wasm_case(seed, &mut rng, guest, &wits[at], contract),
                2 => compose_case(seed, &mut rng, guest, &wits[at], contract),
                _ => graph_case(seed, &mut rng, &base, &contracts),
            }
        }
    });
    std::panic::set_hook(hook);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    let stages = [
        ("WIT parsed", &REACHED.wit_parsed),
        ("WIT lowered", &REACHED.wit_lowered),
        ("a component lowered", &REACHED.component_lowered),
        ("a hub composed", &REACHED.composed),
        ("a graph parsed", &REACHED.graph_parsed),
        ("a graph compiled", &REACHED.graph_compiled),
    ];
    for (stage, reached) in stages {
        let reached = reached.load(Ordering::Relaxed);
        eprintln!("{stage}: {reached}");
        assert!(single || reached > 0, "no input got as far as: {stage}");
    }
}
