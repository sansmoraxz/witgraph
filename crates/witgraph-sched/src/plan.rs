//! Static wiring of the islands, computed once when a graph is loaded.

use std::collections::{BTreeSet, HashMap, HashSet};

use witgraph_ir::{
    CompiledGraph, Connection, ConnectionId, FeedbackIdx, Idx, NodeId, NodeIdx, PortRef, PortTable,
};

use crate::error::RuntimeError;

pub use witgraph_traits::plan::*;

/// The graph's connections, indexed by port.
pub(crate) struct Wiring<'a> {
    /// The non-feedback connection writing each input port.
    pub(crate) writer: HashMap<&'a PortRef, &'a Connection>,
    /// The non-feedback connections reading each output port.
    readers: HashMap<&'a PortRef, Vec<&'a Connection>>,
    /// Every feedback connection.
    feedback: Vec<&'a Connection>,
    /// The connections that unwrap an option
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    unwraps: HashSet<&'a ConnectionId>,
}

impl<'a> Wiring<'a> {
    pub(crate) fn new(compiled: &'a CompiledGraph) -> Self {
        let mut writer = HashMap::new();
        let mut readers: HashMap<&PortRef, Vec<&Connection>> = HashMap::new();
        let mut feedback = Vec::new();
        let mut unwraps = HashSet::new();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            if resolved.unwraps_option {
                unwraps.insert(&conn.id);
            }
            if conn.feedback {
                feedback.push(conn);
            } else {
                writer.insert(&conn.to, conn);
                readers.entry(&conn.from).or_default().push(conn);
            }
        }
        Self {
            writer,
            readers,
            feedback,
            unwraps,
        }
    }
}

/// Computes island `index`'s static wiring from its members' contracts and
/// the shapes of their `run`s. A shape naming an input its contract does
/// not have is [`RuntimeError::InvalidConfig`].
pub(crate) fn plan_island(
    compiled: &CompiledGraph,
    wiring: &Wiring<'_>,
    table: &PortTable,
    index: usize,
    members: &[NodeId],
    shapes: &[RunShape],
) -> Result<IslandPlan, RuntimeError> {
    if shapes.len() != members.len() {
        return Err(RuntimeError::InvalidConfig {
            message: format!(
                "island {index} has {} members but {} run shapes",
                members.len(),
                shapes.len()
            ),
        });
    }
    let position: HashMap<&NodeId, usize> =
        members.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let mut plans = Vec::with_capacity(members.len());
    for (i, node) in members.iter().enumerate() {
        let unknown = || RuntimeError::UnknownNode { node: node.clone() };
        let contract = compiled.contract_for(node).ok_or_else(unknown)?;
        let shape = shapes.get(i).ok_or_else(unknown)?;
        let internal_producer = |port: &str| {
            let conn = wiring
                .writer
                .get(&PortRef::new(node.clone(), port.to_string()))?;
            position.get(&conn.from.node).copied()
        };

        // A shape names the contract's inputs: consumers index its fields
        // as it lists them, so one it does not know would shift them.
        if let Some(name) = shape
            .inputs
            .iter()
            .flatten()
            .find(|name| !contract.inputs.iter().any(|p| p.name.as_str() == *name))
        {
            return Err(RuntimeError::InvalidConfig {
                message: format!(
                    "`{node}`'s `run` takes `{name}`, which its contract has no input for"
                ),
            });
        }
        let mut deps = BTreeSet::new();
        let inputs = shape.inputs.as_ref().map(|fields| {
            fields
                .iter()
                .filter_map(|name| {
                    let port = contract.inputs.iter().find(|p| p.name.as_str() == name)?;
                    let producer = internal_producer(name);
                    if let Some(p) = producer {
                        deps.insert(p);
                    }
                    Some(InputField {
                        name: port.name.clone(),
                        port: PortRef::new(node.clone(), port.name.clone()),
                        index: table.input(node, &port.name)?,
                        kind: port.kind,
                        optional: port.optional,
                        source: if producer.is_some() {
                            InputSource::Member
                        } else {
                            InputSource::External
                        },
                    })
                })
                .collect()
        });

        let outputs = contract
            .outputs
            .iter()
            .filter_map(|port| {
                let consumers = wiring
                    .readers
                    .get(&PortRef::new(node.clone(), port.name.clone()))
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        let member = *position.get(&c.to.node)?;
                        let field = shapes
                            .get(member)?
                            .inputs
                            .as_ref()?
                            .iter()
                            .position(|name| c.to.port.as_str() == name)?;
                        Some(Consumer {
                            member,
                            field,
                            unwrap_option: wiring.unwraps.contains(&c.id),
                        })
                    })
                    .collect();
                Some((
                    port.name.clone(),
                    OutputField {
                        index: table.output(node, &port.name)?,
                        kind: port.kind,
                        consumers,
                    },
                ))
            })
            .collect();

        plans.push(MemberPlan {
            node: node.clone(),
            inputs,
            has_result: shape.has_result,
            outputs,
            deps: deps.into_iter().collect(),
            dependents: Vec::new(),
        });
    }
    for i in 0..plans.len() {
        for d in plans[i].deps.clone() {
            plans[d].dependents.push(i);
        }
    }
    Ok(IslandPlan {
        index,
        members: plans,
    })
}

/// An island's place in the graph-wide node index.
pub(crate) struct IslandWiring {
    /// Where its members start in the graph-wide node index.
    pub(crate) offset: NodeIdx,
    /// Per member, the nodes writing one of its inputs over a non-feedback
    /// connection.
    pub(crate) preds: Vec<Vec<NodeIdx>>,
    /// The nodes writing one of its inputs over a feedback connection.
    pub(crate) feedback_sources: Vec<NodeIdx>,
    /// The feedback connections out of its members.
    pub(crate) feedback_out: Vec<FeedbackIdx>,
}

/// Every island's [`IslandWiring`], in island order.
pub(crate) fn node_wiring(
    compiled: &CompiledGraph,
    wiring: &Wiring<'_>,
    table: &PortTable,
) -> Vec<IslandWiring> {
    // An island's wiring while it is collected: writers as sets, each
    // once, listed when complete.
    struct Collecting {
        offset: NodeIdx,
        preds: Vec<BTreeSet<NodeIdx>>,
        feedback_sources: BTreeSet<NodeIdx>,
        feedback_out: Vec<FeedbackIdx>,
    }
    let mut position: HashMap<&NodeId, (usize, usize, NodeIdx)> = HashMap::new();
    let mut wired = Vec::with_capacity(compiled.islands().len());
    let mut next = 0;
    for (island, members) in compiled.islands().iter().enumerate() {
        for (member, node) in members.iter().enumerate() {
            position.insert(node, (island, member, NodeIdx::at(next + member)));
        }
        wired.push(Collecting {
            offset: NodeIdx::at(next),
            preds: vec![BTreeSet::new(); members.len()],
            feedback_sources: BTreeSet::new(),
            feedback_out: Vec::new(),
        });
        next += members.len();
    }
    for conn in wiring.writer.values() {
        if let (Some(&(_, _, from)), Some(&(island, member, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
        {
            wired[island].preds[member].insert(from);
        }
    }
    for conn in &wiring.feedback {
        if let (Some(&(source, _, from)), Some(&(island, _, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
        {
            if let Some(index) = table.feedback_index(&conn.id) {
                wired[source].feedback_out.push(index);
            }
            wired[island].feedback_sources.insert(from);
        }
    }
    wired
        .into_iter()
        .map(|island| IslandWiring {
            offset: island.offset,
            preds: island
                .preds
                .into_iter()
                .map(|writers| writers.into_iter().collect())
                .collect(),
            feedback_sources: island.feedback_sources.into_iter().collect(),
            feedback_out: island.feedback_out,
        })
        .collect()
}
