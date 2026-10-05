//! Static wiring of the islands, computed once when a graph is loaded.

use std::collections::{BTreeSet, HashMap, HashSet};

use witgraph_ir::{CompiledGraph, Connection, ConnectionId, NodeId, PortKind, PortName, PortRef};

use crate::error::RuntimeError;

/// Where an input field of a member's `inputs` record comes from in a
/// generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    /// Supplied by the host from its latched input values: unconnected
    /// ports, feedback connections, and connections from other islands.
    External,
    /// Produced by another member of the island earlier in the same
    /// generation (routed through [`OutputField::consumers`]).
    Member,
}

/// One field of a member's `inputs` record, in declaration order.
#[derive(Debug, Clone)]
pub struct InputField {
    /// The field's name: the port's.
    pub name: PortName,
    /// The member's port this field is.
    pub port: PortRef,
    /// The port's kind.
    pub kind: PortKind,
    /// Declared `option<T>`: the host wraps the value (or passes `none`).
    pub optional: bool,
    /// Who supplies the field's value.
    pub source: InputSource,
}

/// An in-island reader of an output, over a non-feedback connection.
#[derive(Debug, Clone, Copy)]
pub struct Consumer {
    /// The reading member.
    pub member: usize,
    /// The field of its `inputs` record the connection writes.
    pub field: usize,
    /// Whether the connection unwraps an option
    /// ([`witgraph_ir::PortDef::unwraps_into`]).
    pub unwrap_option: bool,
}

/// One field of a member's `outputs` record.
#[derive(Debug, Clone)]
pub struct OutputField {
    /// The port's kind.
    pub kind: PortKind,
    /// Its in-island consumers. At most one for a stream or future.
    pub consumers: Vec<Consumer>,
}

/// Static wiring of one island member.
#[derive(Debug, Clone)]
pub struct MemberPlan {
    /// The member.
    pub node: NodeId,
    /// The fields of its `inputs` record; `None` when `run` takes none.
    pub inputs: Option<Vec<InputField>>,
    /// Whether `run` returns an `outputs` record.
    pub has_result: bool,
    /// Its output ports.
    pub outputs: HashMap<PortName, OutputField>,
    /// Members whose `run` must return before this one is called.
    pub deps: Vec<usize>,
    /// Members that list this one in their `deps`.
    pub dependents: Vec<usize>,
}

impl MemberPlan {
    /// The member's Value inputs the host supplies (unconnected ones, and
    /// ones fed by feedback connections or from other islands), with their
    /// field index.
    pub fn external_values(&self) -> impl Iterator<Item = (usize, &InputField)> {
        self.inputs
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, f)| f.source == InputSource::External && f.kind == PortKind::Value)
    }

    /// How many fields the member's `inputs` record has.
    pub fn field_count(&self) -> usize {
        self.inputs.as_ref().map_or(0, Vec::len)
    }
}

/// Static wiring of one island. Members are in topological order.
#[derive(Debug, Clone)]
pub struct IslandPlan {
    /// The island's index in
    /// [`CompiledGraph::islands`](witgraph_ir::CompiledGraph::islands).
    pub index: usize,
    /// Its members.
    pub members: Vec<MemberPlan>,
}

impl IslandPlan {
    /// The island's members, sorted: how snapshots name an island.
    pub fn sorted_members(&self) -> Vec<NodeId> {
        let mut members: Vec<NodeId> = self.members.iter().map(|m| m.node.clone()).collect();
        members.sort();
        members
    }

    /// Every member's required external Value inputs.
    pub(crate) fn required_inputs(&self) -> Vec<PortRef> {
        self.members
            .iter()
            .flat_map(|member| {
                member
                    .external_values()
                    .filter(|(_, field)| !field.optional)
                    .map(|(_, field)| field.port.clone())
            })
            .collect()
    }
}

/// The shape of a member's `run` as its component declares it: what an
/// [`Executor`](crate::Executor) tells the scheduler about a node it
/// instantiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunShape {
    /// The names of the `inputs` record's fields, in declaration order;
    /// `None` when `run` takes no parameter.
    pub inputs: Option<Vec<String>>,
    /// Whether `run` returns a result.
    pub has_result: bool,
}

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
            .map(|port| {
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
                (
                    port.name.clone(),
                    OutputField {
                        kind: port.kind,
                        consumers,
                    },
                )
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
    pub(crate) offset: usize,
    /// Per member, the nodes (graph-wide index) writing one of its inputs
    /// over a non-feedback connection.
    pub(crate) preds: Vec<Vec<usize>>,
    /// The nodes (graph-wide index) writing one of its inputs over a
    /// feedback connection.
    pub(crate) feedback_sources: Vec<usize>,
    /// The feedback connections out of its members.
    pub(crate) feedback_out: Vec<ConnectionId>,
}

/// Every island's [`IslandWiring`], in island order.
pub(crate) fn node_wiring(compiled: &CompiledGraph, wiring: &Wiring<'_>) -> Vec<IslandWiring> {
    let mut position: HashMap<&NodeId, (usize, usize, usize)> = HashMap::new();
    let mut wired = Vec::with_capacity(compiled.islands().len());
    let mut next = 0;
    for (island, members) in compiled.islands().iter().enumerate() {
        for (member, node) in members.iter().enumerate() {
            position.insert(node, (island, member, next + member));
        }
        wired.push(IslandWiring {
            offset: next,
            preds: vec![Vec::new(); members.len()],
            feedback_sources: Vec::new(),
            feedback_out: Vec::new(),
        });
        next += members.len();
    }
    let mut seen: HashSet<(usize, usize, usize)> = HashSet::new();
    for conn in wiring.writer.values() {
        if let (Some(&(_, _, from)), Some(&(island, member, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
            && seen.insert((island, member, from))
        {
            wired[island].preds[member].push(from);
        }
    }
    for conn in &wiring.feedback {
        if let (Some(&(source, _, from)), Some(&(island, _, _))) =
            (position.get(&conn.from.node), position.get(&conn.to.node))
        {
            wired[source].feedback_out.push(conn.id.clone());
            if !wired[island].feedback_sources.contains(&from) {
                wired[island].feedback_sources.push(from);
            }
        }
    }
    wired
}
