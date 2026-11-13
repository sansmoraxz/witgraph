//! Fuzzes what witgraph does with a graph. `cargo fuzz` looks for inputs with coverage
//! guidance, and `cargo test` runs the same checks on stable, over seeded inputs and every
//! graph that once failed (`fuzz/regressions`). See `fuzz/README.md`.
//!
//! "It didn't panic" says little: each graph here is also held to rules worked out
//! apart from the compiler, and to what must hold whatever the rules are.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod catalog;
pub mod generate;
pub mod rules;
pub mod run;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use arbitrary::Unstructured;
use miette::Diagnostic as _;
use witgraph_ir::{CompiledGraph, ComponentContract, Graph, PortTable};

use crate::catalog::COMPONENTS;
use crate::rules::{Model, Said, is_local, observed};

/// How far inputs got, as a run whose inputs all fail at the door proves nothing.
#[derive(Debug, Default)]
pub struct Reached {
    /// Graphs parsed.
    pub parsed: AtomicUsize,
    /// Graphs that compiled.
    pub compiled: AtomicUsize,
    /// Graphs the whole-graph rules were compared on.
    pub global: AtomicUsize,
    /// Graphs that compiled with stream groups.
    pub grouped: AtomicUsize,
    /// How often each diagnostic was said, as a rule never reached is a rule untested.
    pub codes: Mutex<BTreeMap<String, usize>>,
}

/// The counts so far.
pub static REACHED: Reached = Reached {
    parsed: AtomicUsize::new(0),
    compiled: AtomicUsize::new(0),
    global: AtomicUsize::new(0),
    grouped: AtomicUsize::new(0),
    codes: Mutex::new(BTreeMap::new()),
};

fn count(stage: &AtomicUsize) {
    stage.fetch_add(1, Ordering::Relaxed);
}

fn contracts() -> Vec<ComponentContract> {
    COMPONENTS
        .iter()
        .map(catalog::Component::contract)
        .collect()
}

/// A graph from a generator that mostly wires it correctly.
pub fn graph_model(data: &[u8]) {
    let mut u = Unstructured::new(data);
    if let Ok(graph) = generate::graph(&mut u) {
        check(&graph);
    }
}

/// A graph, and if it compiles, running it.
pub fn graph_run(data: &[u8]) {
    let mut u = Unstructured::new(data);
    let Ok(graph) = generate::graph(&mut u) else {
        return;
    };
    if let Ok(compiled) = graph.clone().compile(contracts().as_slice()) {
        count(&REACHED.compiled);
        run::run(&graph, compiled, &mut u);
    }
}

/// A graph's JSON that once failed: everything is checked, and it runs if it compiles.
pub fn regression(json: &[u8]) {
    let graph: Graph = serde_json::from_slice(json).expect("a regression is a graph");
    check(&graph);
    if let Ok(compiled) = graph.clone().compile(contracts().as_slice()) {
        run::run(&graph, compiled, &mut Unstructured::new(&[]));
    }
}

/// Bytes, as a graph's JSON.
pub fn graph_json(data: &[u8]) {
    if let Ok(graph) = serde_json::from_slice::<Graph>(data) {
        check(&graph);
    }
}

pub(crate) fn fail(what: &str, graph: &Graph, detail: impl std::fmt::Debug) -> ! {
    let json = serde_json::to_string(graph).unwrap_or_default();
    panic!("{what}: {detail:#?}\ngraph: {json}");
}

fn said_by(diagnostics: &witgraph_ir::Diagnostics) -> BTreeSet<Said> {
    diagnostics.iter().filter_map(observed).collect()
}

/// Everything that must hold of a graph.
pub fn check(graph: &Graph) {
    count(&REACHED.parsed);
    let contracts = contracts();
    round_trips(graph);

    let result = graph.clone().compile(contracts.as_slice());
    let (said, accepted) = match &result {
        Ok(compiled) => (said_by(compiled.warnings()), true),
        Err(failure) => {
            if *failure.graph != *graph {
                fail("a failure gives back another graph", graph, &failure.graph);
            }
            if failure.diagnostics.errors().count() == 0 {
                fail("failed with no error", graph, &failure.diagnostics);
            }
            (said_by(&failure.diagnostics), false)
        }
    };
    speaks_sensibly(graph, &result);
    same_in_any_order(graph, &contracts, &result);
    if let Some(model) = Model::new(graph) {
        follows_the_rules(graph, &model, &said, accepted);
    }
    if let Ok(compiled) = &result {
        count(&REACHED.compiled);
        holds_when_compiled(graph, compiled);
    }
}

fn round_trips(graph: &Graph) {
    let json = serde_json::to_string(graph).expect("a graph serializes");
    let back: Graph = match serde_json::from_str(&json) {
        Ok(back) => back,
        Err(error) => fail("a graph's own JSON does not parse", graph, error),
    };
    if back != *graph {
        fail("a graph changes through JSON", graph, back);
    }
}

/// Diagnostics display, and point at what is there.
fn speaks_sensibly(graph: &Graph, result: &Result<CompiledGraph, witgraph_ir::CompilationFailure>) {
    let diagnostics = match result {
        Ok(compiled) => compiled.warnings(),
        Err(failure) => {
            let _ = failure.to_string();
            &failure.diagnostics
        }
    };
    let mut reached = REACHED.codes.lock().unwrap();
    for (code, times) in codes(diagnostics) {
        *reached.entry(code).or_default() += times;
    }
    drop(reached);
    for diagnostic in diagnostics.iter() {
        let message = diagnostic.to_string();
        if message.is_empty() {
            fail("a diagnostic says nothing", graph, diagnostic);
        }
        if diagnostic.code().is_none() {
            fail("a diagnostic has no code", graph, diagnostic);
        }
        let _ = diagnostic.help().map(|help| help.to_string());
        let _ = diagnostic.location();
    }
}

fn codes(diagnostics: &witgraph_ir::Diagnostics) -> BTreeMap<String, usize> {
    let mut found = BTreeMap::new();
    for diagnostic in diagnostics.iter() {
        let code = diagnostic.code().map(|c| c.to_string()).unwrap_or_default();
        *found.entry(code).or_default() += 1;
    }
    found
}

/// A graph is a set of nodes and connections: the order of declaring them may choose
/// among ties, but not whether it compiles, or what kinds of thing are wrong.
fn same_in_any_order(
    graph: &Graph,
    contracts: &[ComponentContract],
    result: &Result<CompiledGraph, witgraph_ir::CompilationFailure>,
) {
    // A repeated id means its first declaration, by design: there order decides.
    let mut ids = BTreeSet::new();
    if !graph.nodes.iter().all(|node| ids.insert(&node.id)) {
        return;
    }
    let mut reversed = graph.clone();
    reversed.nodes.reverse();
    reversed.connections.reverse();
    let other = reversed.clone().compile(contracts);
    match (result, &other) {
        (Ok(first), Ok(second)) => {
            let found = |c: &CompiledGraph| {
                let mut groups: Vec<BTreeSet<String>> = c
                    .stream_groups()
                    .iter()
                    .map(|group| group.iter().map(ToString::to_string).collect())
                    .collect();
                groups.sort();
                groups
            };
            if found(first) != found(second) {
                fail("declaration order changes the groups", graph, found(second));
            }
            if codes(first.warnings()) != codes(second.warnings()) {
                fail(
                    "declaration order changes the warnings",
                    graph,
                    second.warnings(),
                );
            }
        }
        (Err(first), Err(second)) => {
            let found = |d: &witgraph_ir::Diagnostics| {
                d.errors()
                    .filter_map(|e| e.code().map(|c| c.to_string()))
                    .collect::<BTreeSet<_>>()
            };
            if found(&first.diagnostics) != found(&second.diagnostics) {
                fail(
                    "declaration order changes what is wrong",
                    graph,
                    (&first.diagnostics, &second.diagnostics),
                );
            }
        }
        (Ok(_), Err(failure)) => fail("compiles only in one order", graph, &failure.diagnostics),
        (Err(failure), Ok(_)) => fail("compiles only reversed", graph, &failure.diagnostics),
    }
}

fn follows_the_rules(graph: &Graph, model: &Model<'_>, said: &BTreeSet<Said>, accepted: bool) {
    let expected_local = model.local();
    let said_local: BTreeSet<Said> = said.iter().filter(|(k, _)| is_local(k)).cloned().collect();
    if said_local != expected_local {
        let missed: Vec<_> = expected_local.difference(&said_local).collect();
        let extra: Vec<_> = said_local.difference(&expected_local).collect();
        fail("local rules disagree", graph, (missed, extra));
    }
    if Model::has_local_errors(&expected_local) {
        if accepted {
            fail("compiled with something wrong", graph, expected_local);
        }
        return;
    }
    count(&REACHED.global);
    let expected = model.global();
    let said_global: BTreeSet<Said> = said.iter().filter(|(k, _)| !is_local(k)).cloned().collect();
    if said_global != expected {
        let missed: Vec<_> = expected.difference(&said_global).collect();
        let extra: Vec<_> = said_global.difference(&expected).collect();
        fail("whole-graph rules disagree", graph, (missed, extra));
    }
    let errors_expected = expected.iter().any(|(kind, _)| *kind != "useless_feedback");
    if errors_expected == accepted {
        fail("accepted the wrong way", graph, (accepted, expected));
    }
}

fn holds_when_compiled(graph: &Graph, compiled: &CompiledGraph) {
    let position: BTreeMap<String, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.id.to_string(), at))
        .collect();

    // Earliest declared first among those ready, over non-feedback connections.
    let mut remaining: BTreeMap<usize, usize> = (0..graph.nodes.len()).map(|at| (at, 0)).collect();
    for connection in graph.connections.iter().filter(|c| !c.feedback) {
        *remaining
            .get_mut(&position[connection.to.node.as_str()])
            .unwrap() += 1;
    }
    let mut order = Vec::new();
    while let Some((&next, _)) = remaining.iter().find(|(_, waiting)| **waiting == 0) {
        remaining.remove(&next);
        for connection in graph.connections.iter().filter(|c| !c.feedback) {
            if position[connection.from.node.as_str()] == next {
                *remaining
                    .get_mut(&position[connection.to.node.as_str()])
                    .unwrap() -= 1;
            }
        }
        order.push(graph.nodes[next].id.to_string());
    }
    let topological: Vec<String> = compiled
        .topological_order()
        .iter()
        .map(ToString::to_string)
        .collect();
    if topological != order {
        fail("not the documented order", graph, (topological, order));
    }

    // Depth: the longest path in.
    let mut depth: BTreeMap<String, usize> = BTreeMap::new();
    for node in &order {
        let deepest = graph
            .connections
            .iter()
            .filter(|c| !c.feedback && c.to.node.as_str() == node)
            .map(|c| depth[c.from.node.as_str()] + 1)
            .max()
            .unwrap_or(0);
        depth.insert(node.clone(), deepest);
    }
    let found: BTreeMap<String, usize> = compiled
        .depth_map()
        .into_iter()
        .map(|(node, at)| (node.to_string(), at))
        .collect();
    if found != depth {
        fail("not the documented depths", graph, (found, depth));
    }

    // Groups, and that each node knows its own.
    if !compiled.stream_groups().is_empty() {
        count(&REACHED.grouped);
    }
    for (index, group) in compiled.stream_groups().iter().enumerate() {
        for node in group {
            if compiled.group_of(node) != Some(index) {
                fail("a node does not know its group", graph, node);
            }
        }
    }

    // The port table numbers every port once, and finds each by name.
    let table = PortTable::new(compiled);
    if table.node_count() != graph.nodes.len() {
        fail("the table lost a node", graph, table.node_count());
    }
    let mut inputs = 0;
    let mut outputs = 0;
    for node in compiled.nodes() {
        let contract = compiled
            .contract_for(&node.node.id)
            .expect("a compiled node has a contract");
        for input in &contract.inputs {
            inputs += 1;
            if table.input(&node.node.id, &input.name).is_none() {
                fail("the table lost an input", graph, &input.name);
            }
        }
        for output in &contract.outputs {
            outputs += 1;
            if table.output(&node.node.id, &output.name).is_none() {
                fail("the table lost an output", graph, &output.name);
            }
        }
    }
    if table.inputs().len() != inputs || table.outputs().len() != outputs {
        fail(
            "the table counts ports wrong",
            graph,
            (table.inputs().len(), inputs),
        );
    }
    let mut feedback: Vec<String> = graph
        .connections
        .iter()
        .filter(|c| c.feedback)
        .map(|c| c.id.to_string())
        .collect();
    feedback.sort();
    let numbered: Vec<String> = table.feedback().iter().map(|f| f.id.to_string()).collect();
    if numbered != feedback {
        fail(
            "feedback is not numbered by id",
            graph,
            (numbered, feedback),
        );
    }
}
