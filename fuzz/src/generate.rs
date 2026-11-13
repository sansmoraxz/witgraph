//! Graphs from fuzzer bytes. Purely random graphs are rejected for their first typo and
//! never reach what comes after, so this wires most inputs correctly, then damages a
//! few: the fuzzer can find its way to deep, nearly valid graphs and both sides of
//! every rule.

use std::collections::BTreeMap;

use arbitrary::{Result, Unstructured};
use witgraph_ir::{
    Capacity, Connection, Graph, GraphMetadata, Node, PortRef, ResourceClaim, ResourceDecl,
};

use crate::catalog::{COMPONENTS, Component};

const MAX_NODES: usize = 12;

/// The claims nodes make, in thousandths, so the rules can add them exactly.
const CLAIMS: [u32; 9] = [100, 200, 250, 300, 500, 700, 1000, 1500, 2000];

const RESOURCES: [&str; 2] = ["gpu", "disk"];

/// In thousandths, as claims are.
const CAPACITIES: [u32; 5] = [500, 1000, 1500, 2000, 4000];

/// Port names to get wrong with, which include those of other kinds of node.
const PORT_NAMES: [&str; 8] = ["in", "out", "items", "add", "tensor", "done", "sum", "nope"];

/// Which components a graph mixes: one of everything mostly fails on the wiring, while
/// these reach groups, cycles and handles often.
const PALETTES: [&[&str]; 4] = [
    &[
        "source",
        "relay",
        "gate",
        "text",
        "both",
        "stream-source",
        "stream-tee",
        "stream-tee",
        "stream-pass",
        "stream-sink",
        "stream-waits",
        "text-stream-source",
        "future-source",
        "future-sink",
        "tensor-source",
        "tensor-pass",
        "tensor-reader",
        "tensor-two",
        "blob-source",
    ],
    &["source", "relay", "gate", "relay", "relay"],
    &[
        "stream-source",
        "stream-tee",
        "stream-tee",
        "stream-pass",
        "stream-sink",
        "stream-waits",
        "future-source",
        "future-sink",
        "relay",
        "source",
    ],
    &[
        "tensor-source",
        "tensor-pass",
        "tensor-reader",
        "tensor-two",
        "relay",
        "source",
    ],
];

fn pick<'a, T>(u: &mut Unstructured<'_>, from: &'a [T]) -> Result<&'a T> {
    u.choose(from)
}

/// A graph, and which component each of its node ids first meant.
pub fn graph(u: &mut Unstructured<'_>) -> Result<Graph> {
    let count = u.int_in_range(0..=MAX_NODES)?;
    let palette = *pick(u, &PALETTES)?;
    let mut picked: Vec<&'static Component> = Vec::with_capacity(count);
    let mut nodes = Vec::with_capacity(count);
    for at in 0..count {
        let world = *pick(u, palette)?;
        let component = COMPONENTS
            .iter()
            .find(|component| component.world == world)
            .expect("a palette names components of the catalog");
        picked.push(component);
        let mut resources = BTreeMap::new();
        if u.ratio(1, 3)? {
            let thousandths = *pick(u, &CLAIMS)?;
            let claim = ResourceClaim::new(f64::from(thousandths) / 1000.0)
                .expect("every claim here is an amount");
            resources.insert((*pick(u, &RESOURCES)?).into(), claim);
        }
        let reference = component.reference();
        nodes.push(Node {
            id: format!("n{at}").into(),
            component: reference.component,
            world: reference.world,
            label: None,
            resources,
            attrs: BTreeMap::new(),
        });
    }

    let mut connections = wired(u, &picked)?;

    // Components the table lists.
    let mut components = BTreeMap::new();
    for component in &picked {
        let name = component.reference().component;
        components.insert(name.clone(), name.to_string());
    }

    let mut graph = Graph {
        metadata: GraphMetadata {
            name: "fuzz".into(),
            description: None,
            attrs: BTreeMap::new(),
        },
        components: BTreeMap::new(),
        nodes,
        connections: vec![],
        links: vec![],
        resources: BTreeMap::new(),
    };
    // Mostly what a resource has by default, but with less and with more too.
    for resource in RESOURCES {
        if u.ratio(1, 4)? {
            let capacity = f64::from(*pick(u, &CAPACITIES)?) / 1000.0;
            let capacity = Capacity::new(capacity).expect("every one here is a capacity");
            graph
                .resources
                .insert(resource.into(), ResourceDecl { capacity });
        }
    }
    damage(u, &mut connections, &mut graph, count)?;
    graph.connections = connections;
    graph.components = components;
    if u.ratio(1, 64)? {
        graph.components.pop_last();
    }
    Ok(graph)
}

fn connection(id: usize, from: PortRef, to: PortRef, feedback: bool) -> Connection {
    Connection {
        id: format!("c{id}").into(),
        from,
        to,
        feedback,
        attrs: BTreeMap::new(),
    }
}

/// Connects most inputs from an output that fits: an earlier node's, or now and then
/// any node's over a feedback connection, so that cycles come with their boundary.
fn wired(u: &mut Unstructured<'_>, picked: &[&'static Component]) -> Result<Vec<Connection>> {
    let mut connections = Vec::new();
    let mut moved: Vec<(usize, &'static str)> = Vec::new();
    // How given to loops this graph is, and whether all of them have their boundary.
    let (back_in, leave_unmarked) = (*pick(u, &[32u8, 8, 3])?, u.ratio(1, 8)?);
    for (at, component) in picked.iter().enumerate() {
        for input in component.inputs {
            let wanted = if input.optional {
                u.ratio(1, 3)?
            } else {
                !u.ratio(1, 32)?
            };
            if !wanted {
                continue;
            }
            let backwards = u.ratio(1, back_in)?;
            let mut sources = Vec::new();
            for (source, other) in picked.iter().enumerate() {
                if !backwards && source >= at {
                    continue;
                }
                for output in other.outputs {
                    let fits = output.kind == input.kind && output.payload.fits(input);
                    let free = !output.kind.moves() || !moved.contains(&(source, output.name));
                    if fits && free {
                        sources.push((source, output));
                    }
                }
            }
            if sources.is_empty() {
                continue;
            }
            let (source, output) = *pick(u, &sources)?;
            if output.kind.moves() {
                moved.push((source, output.name));
            }
            let feedback = source >= at && !leave_unmarked && !output.kind.moves();
            let id = connections.len();
            connections.push(connection(
                id,
                PortRef::new(format!("n{source}"), output.name),
                PortRef::new(format!("n{at}"), input.name),
                feedback,
            ));
        }
    }
    Ok(connections)
}

/// A few changes of the kind a person's edit or a bad file makes.
fn damage(
    u: &mut Unstructured<'_>,
    connections: &mut Vec<Connection>,
    graph: &mut Graph,
    count: usize,
) -> Result<()> {
    let mut remaining = if u.ratio(1, 2)? {
        0
    } else {
        u.int_in_range(1..=4u8)?
    };
    while remaining > 0 {
        remaining -= 1;
        let node = |u: &mut Unstructured<'_>| -> Result<String> {
            if u.ratio(1, 16)? {
                return Ok("ghost".into());
            }
            Ok(format!("n{}", u.int_in_range(0..=count.max(1) - 1)?))
        };
        match u.int_in_range(0..=6u8)? {
            0 => {
                let id = connections.len() + 100;
                let from = PortRef::new(node(u)?, *pick(u, &PORT_NAMES)?);
                let to = PortRef::new(node(u)?, *pick(u, &PORT_NAMES)?);
                let feedback = u.arbitrary()?;
                connections.push(connection(id, from, to, feedback));
            }
            1 if !connections.is_empty() => {
                let at = u.choose_index(connections.len())?;
                connections.remove(at);
            }
            2 if !connections.is_empty() => {
                let at = u.choose_index(connections.len())?;
                connections[at].feedback = !connections[at].feedback;
            }
            3 if !connections.is_empty() => {
                let at = u.choose_index(connections.len())?;
                let mut copy = connections[at].clone();
                if u.arbitrary()? {
                    copy.id = format!("c{}", connections.len() + 200).into();
                }
                connections.push(copy);
            }
            4 if !connections.is_empty() => {
                let at = u.choose_index(connections.len())?;
                connections[at].to = PortRef::new(node(u)?, *pick(u, &PORT_NAMES)?);
            }
            5 if !connections.is_empty() => {
                let at = u.choose_index(connections.len())?;
                let other = u.choose_index(connections.len())?;
                connections[at].id = connections[other].id.clone();
            }
            6 if !graph.nodes.is_empty() => {
                let at = u.choose_index(graph.nodes.len())?;
                let other = u.choose_index(graph.nodes.len())?;
                graph.nodes[at].id = graph.nodes[other].id.clone();
            }
            _ => {}
        }
    }
    Ok(())
}
