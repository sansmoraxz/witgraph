//! Static wiring of the islands, computed once when a graph is loaded.

use std::collections::HashMap;

use witgraph_ir::{InputIdx, NodeId, OutputIdx, PortKind, PortName, PortRef};

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
    /// The same port, numbered.
    pub index: InputIdx,
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
    /// The port, numbered.
    pub index: OutputIdx,
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
    /// The member's Value and handle inputs the host supplies (unconnected
    /// ones, and ones fed by feedback connections or from other islands),
    /// with their field index.
    pub fn external_values(&self) -> impl Iterator<Item = (usize, &InputField)> {
        self.inputs
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, f)| f.source == InputSource::External && f.kind.is_latched())
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
    pub fn required_inputs(&self) -> Vec<InputIdx> {
        self.members
            .iter()
            .flat_map(|member| {
                member
                    .external_values()
                    .filter(|(_, field)| !field.optional)
                    .map(|(_, field)| field.index)
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
