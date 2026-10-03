//! The graph IR: nodes, connections, metadata, and the builder.
//!
//! A [`Graph`] references the components its nodes instantiate by
//! [`ComponentRef`] (content hash included) and never embeds their
//! contracts: contracts are always re-derived from WIT and supplied to
//! [`Graph::compile`] through a [`ContractSource`](crate::ContractSource). A
//! serialized graph therefore carries no type information that could drift
//! from the components it names.

use std::collections::BTreeMap;

use crate::id::{ComponentRef, ConnectionId, NodeId, PortRef, ResourceId};

/// A validated fraction in `(0.0, 1.0]`, representing the share of a
/// named resource pool a node consumes while active.
///
/// Construction validates the invariant; invalid values cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Fraction(f64);

impl Fraction {
    /// Creates a fraction, returning an error if the value is outside
    /// `(0.0, 1.0]` or is NaN/Infinity.
    pub fn new(value: f64) -> Result<Self, String> {
        if !(value > 0.0 && value <= 1.0) {
            return Err(format!("fraction {value} is outside (0.0, 1.0]"));
        }
        Ok(Self(value))
    }

    /// Returns the inner `f64`.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl core::fmt::Display for Fraction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Fraction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Fraction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Fraction::new(value).map_err(serde::de::Error::custom)
    }
}

/// A fractional claim on a named shared resource.
///
/// Each named resource pool has an implicit capacity of 1.0. The fraction
/// is validated when the claim is constructed (or deserialized). The
/// runtime sums the claims of every node in an island, holds them for as
/// long as the island's generation runs, and defers starting an island
/// while the sum of held claims on any resource would exceed 1.0; an island
/// whose own claims exceed 1.0 is rejected when the graph is loaded.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ResourceClaim {
    /// Fraction of the resource consumed while running.
    pub fraction: Fraction,
}

impl ResourceClaim {
    /// A claim on `fraction` of a resource, which must be in `(0.0, 1.0]`.
    pub fn new(fraction: f64) -> Result<Self, String> {
        Ok(Self {
            fraction: Fraction::new(fraction)?,
        })
    }
}

/// Human-facing information about a graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
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
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Node {
    /// The node's graph-unique id.
    pub id: NodeId,
    /// The component this node instantiates.
    pub component: ComponentRef,
    /// Display label, if it differs from the id.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub label: Option<String>,
    /// Fractional claims on named shared resources. The runtime sums them
    /// per island and holds them while the island's generation runs; see
    /// [`ResourceClaim`].
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub resources: BTreeMap<ResourceId, ResourceClaim>,
}

/// A directed edge from an output port to an input port.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Connection {
    /// The connection's graph-unique id.
    pub id: ConnectionId,
    /// Must resolve to an output port.
    pub from: PortRef,
    /// Must resolve to an input port.
    pub to: PortRef,
    /// Marks a state boundary: this edge delivers the previous iteration's
    /// value (unit delay). Cycles are legal iff every cycle contains at
    /// least one feedback edge, and feedback edges must carry Value ports.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub feedback: bool,
}

/// The serializable data stage of the typestate chain: a complete but not
/// yet compiled flow graph.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Graph {
    /// Human-facing information about the graph.
    pub metadata: GraphMetadata,
    /// The component table: every component the nodes may reference,
    /// normally pinned by content hash. A node's [`ComponentRef`] resolves
    /// against this table (an unhashed node reference matches a hashed
    /// entry); each entry resolves to a contract at compile time.
    pub components: Vec<ComponentRef>,
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

/// [`GraphBuilder::set_resource`] named a node the builder does not have.
/// Carries the builder back, unchanged.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown node `{node}`")]
pub struct UnknownNodeError {
    /// The builder, as it was before the call.
    pub builder: Box<GraphBuilder>,
    /// The node that was not found.
    pub node: NodeId,
}

/// First stage of the typestate chain: `GraphBuilder → Graph →
/// CompiledGraph`. [`GraphBuilder::build`] does not validate;
/// [`Graph::compile`] is the next transition.
#[derive(Debug, Clone)]
pub struct GraphBuilder {
    graph: Graph,
}

impl GraphBuilder {
    /// Adds a component to the graph's component table. Accepts a
    /// [`ComponentRef`] or a contract (`&contract`), whose id is recorded.
    pub fn add_component(mut self, component: impl Into<ComponentRef>) -> Self {
        self.graph.components.push(component.into());
        self
    }

    /// Adds a node instantiating the given component.
    pub fn add_node(mut self, id: impl Into<NodeId>, component: ComponentRef) -> Self {
        self.graph.nodes.push(Node {
            id: id.into(),
            component,
            label: None,
            resources: BTreeMap::new(),
        });
        self
    }

    /// Declares that the named node consumes `claim.fraction` of the
    /// shared resource `resource` while active. Overwrites any previous
    /// claim on the same resource for the same node. Fails, handing the
    /// builder back, when no node has that id yet.
    pub fn set_resource(
        mut self,
        node: impl Into<NodeId>,
        resource: impl Into<ResourceId>,
        claim: ResourceClaim,
    ) -> Result<Self, UnknownNodeError> {
        let node = node.into();
        match self.graph.nodes.iter_mut().find(|n| n.id == node) {
            Some(n) => {
                n.resources.insert(resource.into(), claim);
                Ok(self)
            }
            None => Err(UnknownNodeError {
                builder: Box::new(self),
                node,
            }),
        }
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
    use crate::Type;
    use crate::component::{ComponentContract, RunKind};
    use crate::id::PackageRef;
    use crate::port::{PortDef, PortKind};

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
            inputs: vec![PortDef::new("in", PortKind::Value, Type::STRING)],
            outputs: vec![PortDef::new("out", PortKind::Value, Type::STRING)],
            run: RunKind::Sync,
            capabilities: vec![],
            docs: None,
        };
        let graph = Graph::builder("round-trip")
            .add_component(&component)
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
    fn serialized_graph_holds_only_component_refs() {
        let id: ComponentRef = "demo:graph/echo@0.1.0".parse().unwrap();
        let mut pinned = id.clone();
        pinned.content_hash = Some("aa".into());
        let graph = Graph::builder("refs")
            .add_component(pinned)
            .add_node("n", id)
            .build();
        let json = serde_json::to_value(&graph).unwrap();
        let entry = json["components"][0].as_object().unwrap();
        let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["content_hash", "package", "world"],
            "the table stores refs, never ports or types: {entry:?}"
        );
    }

    #[test]
    fn wire_format_omits_defaults() {
        let graph = Graph::builder("minimal").build();
        let json = serde_json::to_value(&graph).unwrap();
        let metadata = json.get("metadata").unwrap().as_object().unwrap();
        assert!(!metadata.contains_key("description"));
        assert!(!metadata.contains_key("attrs"));

        let conn = Connection {
            id: "c".into(),
            from: PortRef::new("a", "o"),
            to: PortRef::new("b", "i"),
            feedback: false,
        };
        let json = serde_json::to_value(&conn).unwrap();
        assert!(!json.as_object().unwrap().contains_key("feedback"));

        let node = Node {
            id: "n".into(),
            component: ComponentRef {
                package: PackageRef {
                    namespace: "x".into(),
                    name: "y".into(),
                    version: None,
                },
                world: "w".into(),
                content_hash: None,
            },
            label: None,
            resources: BTreeMap::new(),
        };
        let json = serde_json::to_value(&node).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("resources"),
            "empty resources omitted from wire format"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let misspelled = r#"{"package":{"namespace":"d","name":"g","version":null},"world":"w","contentHash":"aa"}"#;
        assert!(
            serde_json::from_str::<ComponentRef>(misspelled).is_err(),
            "a misspelled key must not silently drop the pin"
        );
        let conn = r#"{"id":"c","from":{"node":"a","port":"o"},"to":{"node":"b","port":"i"},"feedbak":true}"#;
        assert!(serde_json::from_str::<Connection>(conn).is_err());
        let node = r#"{"id":"n","component":{"package":{"namespace":"d","name":"g","version":null},"world":"w"},"resource":{}}"#;
        assert!(serde_json::from_str::<Node>(node).is_err());
    }

    #[test]
    fn component_refs_are_validated_on_deserialize() {
        let bad = r#"{"package":{"namespace":"Demo X","name":"g","version":null},"world":"w"}"#;
        assert!(serde_json::from_str::<ComponentRef>(bad).is_err());
        let good =
            r#"{"package":{"namespace":"demo","name":"g","version":null},"world":"stage-2"}"#;
        assert!(serde_json::from_str::<ComponentRef>(good).is_ok());
    }

    #[test]
    fn omitted_flags_default_to_false() {
        let conn: Connection = serde_json::from_str(
            r#"{"id":"c","from":{"node":"a","port":"o"},"to":{"node":"b","port":"i"}}"#,
        )
        .unwrap();
        assert!(!conn.feedback);
    }
}
