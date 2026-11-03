//! A node's static wiring, computed once when a graph is loaded.

use std::collections::HashMap;

use witgraph_ir::{InputIdx, NodeId, NodeIdx, OutputIdx, PortKind, PortName, PortRef};

/// One field of a node's `inputs` record, in declaration order.
#[derive(Debug, Clone)]
pub struct InputField {
    /// The field's name: the port's.
    pub name: PortName,
    /// The node's port this field is.
    pub port: PortRef,
    /// The same port, numbered.
    pub index: InputIdx,
    /// The port's kind.
    pub kind: PortKind,
    /// Declared `option<T>`: the host wraps the value (or passes `none`).
    pub optional: bool,
}

/// One field of a node's `outputs` record.
#[derive(Debug, Clone, Copy)]
pub struct OutputField {
    /// The port, numbered.
    pub index: OutputIdx,
    /// The port's kind.
    pub kind: PortKind,
}

/// How to call one node.
#[derive(Debug, Clone)]
pub struct MemberPlan {
    /// The node.
    pub node: NodeId,
    /// Its position in the graph.
    pub index: NodeIdx,
    /// The fields of its `inputs` record; `None` when `run` takes none.
    pub inputs: Option<Vec<InputField>>,
    /// Whether `run` returns an `outputs` record.
    pub has_result: bool,
    /// Its output ports.
    pub outputs: HashMap<PortName, OutputField>,
}

impl MemberPlan {
    /// How many fields the node's `inputs` record has.
    pub fn field_count(&self) -> usize {
        self.inputs.as_ref().map_or(0, Vec::len)
    }
}

/// The shape of a node's `run` as its component declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunShape {
    /// The names of the `inputs` record's fields, in declaration order;
    /// `None` when `run` takes no parameter.
    pub inputs: Option<Vec<String>>,
    /// Whether `run` returns a result.
    pub has_result: bool,
}
