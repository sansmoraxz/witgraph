//! The graph IR: nodes, connections, metadata, and the builder.
//!
//! A [`Graph`] is self-contained — it embeds the component contracts its
//! nodes reference, so a serialized graph is validatable and compilable with
//! no editor, filesystem, or WIT resolver present.

use std::collections::BTreeMap;

use crate::component::ComponentContract;
use crate::id::{ComponentRef, ConnectionId, NodeId, PortRef};

/// Human-facing information about a graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GraphMetadata {
    /// The graph's name.
    pub name: String,
    /// Free-form description, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub description: Option<String>,
    /// Editor-agnostic extension point.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub attrs: BTreeMap<String, String>,
}

/// An instance of a component in a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Node {
    /// The node's graph-unique id.
    pub id: NodeId,
    /// The component this node instantiates.
    pub component: ComponentRef,
    /// Display label, if it differs from the id.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub label: Option<String>,
}

/// A directed edge from an output port to an input port.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Connection {
    /// The connection's graph-unique id.
    pub id: ConnectionId,
    /// Must resolve to an output port.
    pub from: PortRef,
    /// Must resolve to an input port.
    pub to: PortRef,
    /// Marks a state boundary: this edge delivers the previous iteration's
    /// value (unit delay). Cycles are legal iff every cycle contains at
    /// least one feedback edge.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub feedback: bool,
}

/// The serializable data stage of the typestate chain: a complete but not
/// yet compiled flow graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Graph {
    /// Human-facing information about the graph.
    pub metadata: GraphMetadata,
    /// Embedded contract table for every component the nodes reference.
    pub components: Vec<ComponentContract>,
    /// The graph's nodes.
    pub nodes: Vec<Node>,
    /// The graph's connections.
    pub connections: Vec<Connection>,
}

impl Graph {
    /// Starts a [`GraphBuilder`] for a graph with the given name.
    pub fn builder(name: impl Into<String>) -> GraphBuilder {
        GraphBuilder {
            graph: Graph {
                metadata: GraphMetadata {
                    name: name.into(),
                    ..GraphMetadata::default()
                },
                ..Graph::default()
            },
        }
    }
}

/// First stage of the typestate chain: `GraphBuilder → Graph →
/// CompiledGraph`. [`GraphBuilder::build`] does not validate;
/// [`Graph::compile`] is the next transition.
#[derive(Debug, Clone)]
pub struct GraphBuilder {
    graph: Graph,
}

impl GraphBuilder {
    /// Registers a component contract nodes can reference.
    pub fn add_component(mut self, contract: ComponentContract) -> Self {
        self.graph.components.push(contract);
        self
    }

    /// Adds a node instantiating the given component.
    pub fn add_node(mut self, id: impl Into<NodeId>, component: ComponentRef) -> Self {
        self.graph.nodes.push(Node {
            id: id.into(),
            component,
            label: None,
        });
        self
    }

    /// Connects an output port to an input port.
    pub fn connect(self, id: impl Into<ConnectionId>, from: PortRef, to: PortRef) -> Self {
        self.connect_impl(id, from, to, false)
    }

    /// Connects an output port to an input port across a feedback boundary
    /// (see [`Connection::feedback`]).
    pub fn connect_feedback(self, id: impl Into<ConnectionId>, from: PortRef, to: PortRef) -> Self {
        self.connect_impl(id, from, to, true)
    }

    fn connect_impl(
        mut self,
        id: impl Into<ConnectionId>,
        from: PortRef,
        to: PortRef,
        feedback: bool,
    ) -> Self {
        self.graph.connections.push(Connection {
            id: id.into(),
            from,
            to,
            feedback,
        });
        self
    }

    /// The `GraphBuilder → Graph` typestate transition. Does not validate;
    /// [`Graph::compile`] is the next transition.
    pub fn build(self) -> Graph {
        self.graph
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::id::PackageRef;
    use crate::port::{PortDef, PortKind};
    use crate::types::Type;

    #[test]
    fn serde_round_trip_preserves_ids() {
        let component = ComponentContract {
            id: ComponentRef {
                package: PackageRef {
                    namespace: "demo".into(),
                    name: "graph".into(),
                    version: Some(semver::Version::new(0, 1, 0)),
                },
                world: "echo".into(),
                content_hash: Some("aa".into()),
            },
            inputs: vec![PortDef::new("in", PortKind::Value, Type::String)],
            outputs: vec![PortDef::new("out", PortKind::Value, Type::String)],
            capabilities: vec![],
            type_names: vec![],
            docs: None,
        };
        let graph = Graph::builder("round-trip")
            .add_component(component.clone())
            .add_node("a", component.id.clone())
            .add_node("b", component.id.clone())
            .connect("c1", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect_feedback("c2", PortRef::new("b", "out"), PortRef::new("a", "in"))
            .build();

        let json = serde_json::to_string_pretty(&graph).unwrap();
        let back: Graph = serde_json::from_str(&json).unwrap();
        assert_eq!(graph, back);
        assert!(back.connections[1].feedback);
    }

    #[test]
    fn wire_format_omits_defaults() {
        let graph = Graph::builder("minimal").build();
        let json = serde_json::to_value(&graph).unwrap();
        let metadata = json.get("metadata").unwrap().as_object().unwrap();
        assert!(!metadata.contains_key("description"));
        assert!(!metadata.contains_key("attrs"));

        let port = PortDef::new("in", PortKind::Value, Type::Bool);
        let json = serde_json::to_value(&port).unwrap();
        let port = json.as_object().unwrap();
        assert!(!port.contains_key("optional"));
        assert!(!port.contains_key("drained"));
        assert!(!port.contains_key("docs"));

        let conn = Connection {
            id: "c".into(),
            from: PortRef::new("a", "o"),
            to: PortRef::new("b", "i"),
            feedback: false,
        };
        let json = serde_json::to_value(&conn).unwrap();
        assert!(!json.as_object().unwrap().contains_key("feedback"));
    }

    #[test]
    fn omitted_flags_default_to_false() {
        let port: PortDef =
            serde_json::from_str(r#"{"name":"in","kind":"value","type":"bool"}"#).unwrap();
        assert!(!port.optional && !port.drained);
        let conn: Connection = serde_json::from_str(
            r#"{"id":"c","from":{"node":"a","port":"o"},"to":{"node":"b","port":"i"}}"#,
        )
        .unwrap();
        assert!(!conn.feedback);
    }
}
