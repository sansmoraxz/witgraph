//! The graph IR: nodes, connections, metadata, and the builder.
//!
//! A [`Graph`] names the components its nodes instantiate in a table, each
//! by a [`ComponentName`] and where its node manifest is, and each node the
//! world it runs of one of them. It never embeds their contracts: contracts
//! are always lowered from the components' WIT and supplied to
//! [`Graph::compile`] through a [`ContractSource`](crate::ContractSource). A
//! serialized graph therefore carries no type information that could drift
//! from the components it names.

use std::collections::BTreeMap;

use crate::component::ComponentContract;
use crate::id::{
    ComponentName, ConnectionId, ContractId, LinkId, NodeId, PortRef, ResourceId, WorldRef,
};

/// How much of a named resource a node claims while active: positive and finite.
///
/// Construction validates the invariant; invalid values cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Amount(f64);

impl Amount {
    /// Creates an amount, returning an error if the value is not above 0.0 and finite.
    pub fn new(value: f64) -> Result<Self, String> {
        if !(value > 0.0 && value.is_finite()) {
            return Err(format!("amount {value} is not above 0.0 and finite"));
        }
        Ok(Self(value))
    }

    /// Returns the inner `f64`.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl core::fmt::Display for Amount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Amount {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Amount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Amount::new(value).map_err(serde::de::Error::custom)
    }
}

/// How much of a named resource there is: positive and finite.
///
/// Construction validates the invariant; invalid values cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Capacity(f64);

impl Capacity {
    /// Creates a capacity, returning an error if the value is not above 0.0 and finite.
    pub fn new(value: f64) -> Result<Self, String> {
        if !(value > 0.0 && value.is_finite()) {
            return Err(format!("capacity {value} is not above 0.0 and finite"));
        }
        Ok(Self(value))
    }

    /// Returns the inner `f64`.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for Capacity {
    /// What a resource has that the graph does not declare.
    fn default() -> Self {
        Self(1.0)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Capacity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Capacity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Capacity::new(value).map_err(serde::de::Error::custom)
    }
}

/// What a graph says of a named resource it declares.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ResourceDecl {
    /// How much of it there is, to claim at once.
    pub capacity: Capacity,
}

/// A claim on a named shared resource.
///
/// A resource has a capacity of 1.0 unless the graph declares otherwise
/// ([`Graph::resources`]), and an amount is in the same units: with a capacity of 2.0, a claim
/// of 1.0 is half of it. A node claiming more than there is is rejected when the graph is
/// compiled. Anything else runs: the scheduler starts a node once its claim fits beside what
/// is running. A member of a stream group holds its claim past its `run`, until the group has
/// drained, so nothing else starts in its room; but what the group is itself waiting on may,
/// as the group could never drain otherwise. So a group can go over a capacity while it works.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ResourceClaim {
    /// How much of the resource it holds while running.
    pub amount: Amount,
}

impl ResourceClaim {
    /// A claim on `amount` of a resource, which must be above 0.0 and finite.
    pub fn new(amount: f64) -> Result<Self, String> {
        Ok(Self {
            amount: Amount::new(amount)?,
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
    /// The component this node instantiates, by its name in the graph's
    /// component table.
    pub component: ComponentName,
    /// The world of that component the node runs.
    pub world: WorldRef,
    /// Display label, if it differs from the id.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub label: Option<String>,
    /// Fractional claims on named shared resources, held while the node runs;
    /// see [`ResourceClaim`].
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub resources: BTreeMap<ResourceId, ResourceClaim>,
    /// Editor-agnostic extension point: arbitrary key-value pairs that the
    /// runtime ignores. An editor stores layout here (position, size,
    /// collapsed state); other tools may add their own, namespaced by
    /// convention (e.g. `editor.x`, `editor.y`).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub attrs: BTreeMap<String, String>,
}

impl Node {
    /// The contract the node runs: its world, as its component provides it.
    pub fn contract_id(&self) -> ContractId {
        ContractId::new(self.component.clone(), self.world.clone())
    }
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
    /// Editor-agnostic extension point: arbitrary key-value pairs that the
    /// runtime ignores. An editor may store routing or visual metadata here.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub attrs: BTreeMap<String, String>,
}

/// Satisfies one of a node's imports with another component's export,
/// instead of leaving it to the host as a capability.
///
/// The provider is an ordinary component (it need not be a node). When the
/// graph is loaded it is composed into the node's component, so the node
/// calls it directly: each node gets a provider instance of its own, which
/// lives and is rebuilt with the node. Links of one node that name the same
/// provider share that instance. Whatever the provider itself imports
/// becomes a capability of the node.
///
/// Compilation checks that the node and the import exist; that the export
/// fits the import is checked when the graph is loaded, from the
/// components themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Link {
    /// The link's graph-unique id.
    pub id: LinkId,
    /// The node whose import is satisfied.
    pub node: NodeId,
    /// The import, as the node's contract names the capability
    /// ([`Capability::interface`](crate::Capability::interface)): an
    /// interface id, an inline interface's import name, or a label.
    pub import: String,
    /// The component providing it, by its name in the graph's component
    /// table.
    pub provider: ComponentName,
    /// The provider's export that satisfies the import
    /// (`namespace:name/interface@version`).
    pub export: String,
}

/// An entry of a graph's component table: a component's name, and where
/// its node manifest is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentEntry {
    /// The component's name in the table.
    pub name: ComponentName,
    /// Where its node manifest is: a path, or an `oci://` reference.
    pub location: String,
}

impl ComponentEntry {
    /// `name`, described by the node manifest at `location`.
    pub fn new(name: ComponentName, location: impl Into<String>) -> Self {
        Self {
            name,
            location: location.into(),
        }
    }
}

impl From<&ContractId> for ComponentEntry {
    fn from(id: &ContractId) -> Self {
        let name = id.component.clone();
        Self {
            location: name.to_string(),
            name,
        }
    }
}

impl From<ContractId> for ComponentEntry {
    fn from(id: ContractId) -> Self {
        Self::from(&id)
    }
}

impl From<&ComponentContract> for ComponentEntry {
    fn from(contract: &ComponentContract) -> Self {
        Self::from(&contract.id)
    }
}

impl From<ComponentContract> for ComponentEntry {
    fn from(contract: ComponentContract) -> Self {
        Self::from(&contract.id)
    }
}

/// The serializable data stage of the typestate chain: a complete but not
/// yet compiled flow graph.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Graph {
    /// Human-facing information about the graph.
    pub metadata: GraphMetadata,
    /// The component table: every component the nodes and links may name,
    /// by name, with where its node manifest is (a path, or an `oci://`
    /// reference). The location is not read here: whoever supplies the
    /// contracts and the components resolves it.
    pub components: BTreeMap<ComponentName, String>,
    /// The graph's nodes.
    pub nodes: Vec<Node>,
    /// The graph's connections.
    pub connections: Vec<Connection>,
    /// Imports of nodes satisfied by other components' exports.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub links: Vec<Link>,
    /// How much there is of the named resources nodes claim. One not here has a capacity of 1.0.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub resources: BTreeMap<ResourceId, ResourceDecl>,
}

impl Graph {
    /// How much there is of `resource`.
    #[must_use]
    pub fn capacity_of(&self, resource: &ResourceId) -> f64 {
        self.resources
            .get(resource)
            .map_or(Capacity::default(), |declared| declared.capacity)
            .get()
    }

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
    /// Adds a component to the graph's component table: a
    /// [`ComponentEntry`], or a contract (`&contract`), whose component is
    /// entered under its name, at a location of that name.
    pub fn add_component(mut self, entry: impl Into<ComponentEntry>) -> Self {
        let entry = entry.into();
        self.graph.components.insert(entry.name, entry.location);
        self
    }

    /// Adds a node running the given contract: a world, as a component of
    /// the table provides it.
    pub fn add_node(mut self, id: impl Into<NodeId>, contract: ContractId) -> Self {
        self.graph.nodes.push(Node {
            id: id.into(),
            component: contract.component,
            world: contract.world,
            label: None,
            resources: BTreeMap::new(),
            attrs: BTreeMap::new(),
        });
        self
    }

    /// Says how much there is of a named resource, to claim at once.
    pub fn set_capacity(mut self, resource: impl Into<ResourceId>, capacity: Capacity) -> Self {
        self.graph
            .resources
            .insert(resource.into(), ResourceDecl { capacity });
        self
    }

    /// Declares that the named node consumes `claim.amount` of the
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
            attrs: BTreeMap::new(),
        });
        self
    }

    /// Satisfies `node`'s import `import` with `provider`'s export `export`
    /// (see [`Link`]).
    pub fn link(
        mut self,
        id: impl Into<LinkId>,
        node: impl Into<NodeId>,
        import: impl Into<String>,
        provider: ComponentName,
        export: impl Into<String>,
    ) -> Self {
        self.graph.links.push(Link {
            id: id.into(),
            node: node.into(),
            import: import.into(),
            provider,
            export: export.into(),
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
    use crate::port::PortDef;

    #[test]
    fn serde_round_trip_preserves_ids() {
        let component = ComponentContract {
            id: "echo/demo:graph/echo@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::value("in", Type::STRING)],
            outputs: vec![PortDef::value("out", Type::STRING)],
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
    fn serialized_graph_holds_only_references() {
        let id: ContractId = "echo/demo:graph/echo@0.1.0".parse().unwrap();
        let graph = Graph::builder("refs")
            .add_component(ComponentEntry::new(
                id.component.clone(),
                "oci://example.com/echo:0.1.0",
            ))
            .add_node("n", id)
            .build();
        let json = serde_json::to_value(&graph).unwrap();
        assert_eq!(
            json["components"],
            serde_json::json!({ "echo": "oci://example.com/echo:0.1.0" }),
            "the table maps names to node manifests, never ports or types"
        );
        assert_eq!(json["nodes"][0]["component"], "echo");
        assert_eq!(json["nodes"][0]["world"], "demo:graph/echo@0.1.0");
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
            attrs: BTreeMap::new(),
        };
        let json = serde_json::to_value(&conn).unwrap();
        let conn_obj = json.as_object().unwrap();
        assert!(!conn_obj.contains_key("feedback"));
        assert!(
            !conn_obj.contains_key("attrs"),
            "empty attrs omitted from connection wire format"
        );

        let node = Node {
            id: "n".into(),
            component: "c".parse().unwrap(),
            world: "x:y/w".parse().unwrap(),
            label: None,
            resources: BTreeMap::new(),
            attrs: BTreeMap::new(),
        };
        let json = serde_json::to_value(&node).unwrap();
        let node_obj = json.as_object().unwrap();
        assert!(
            !node_obj.contains_key("resources"),
            "empty resources omitted from wire format"
        );
        assert!(
            !node_obj.contains_key("attrs"),
            "empty attrs omitted from node wire format"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let conn = r#"{"id":"c","from":{"node":"a","port":"o"},"to":{"node":"b","port":"i"},"feedbak":true}"#;
        assert!(serde_json::from_str::<Connection>(conn).is_err());
        let node = r#"{"id":"n","component":"c","world":"d:g/w","resource":{}}"#;
        assert!(serde_json::from_str::<Node>(node).is_err());
    }

    #[test]
    fn references_are_validated_on_deserialize() {
        let node = |component: &str, world: &str| {
            serde_json::from_str::<Node>(&format!(
                r#"{{"id":"n","component":"{component}","world":"{world}"}}"#
            ))
        };
        assert!(node("c", "demo:g/stage-2").is_ok());
        assert!(node("c", "Demo X:g/w").is_err());
        assert!(node("C D", "demo:g/w").is_err());
        let table = r#"{"metadata":{"name":"t"},"components":{"Not A Name":"x"},"nodes":[],"connections":[]}"#;
        assert!(serde_json::from_str::<Graph>(table).is_err());
    }

    #[test]
    fn omitted_flags_default_to_false() {
        let conn: Connection = serde_json::from_str(
            r#"{"id":"c","from":{"node":"a","port":"o"},"to":{"node":"b","port":"i"}}"#,
        )
        .unwrap();
        assert!(!conn.feedback);
    }

    #[test]
    fn capacities_are_read_from_a_graph_and_left_out_when_none_are_declared() {
        let graph = |resources: &str| {
            serde_json::from_str::<Graph>(&format!(
                r#"{{"metadata":{{"name":"t"}},"components":{{}},"nodes":[],"connections":[],
                    "resources":{resources}}}"#
            ))
        };
        let read = graph(r#"{"gpu":{"capacity":2.0}}"#).unwrap();
        assert_eq!(read.capacity_of(&"gpu".into()), 2.0);
        assert_eq!(read.capacity_of(&"disk".into()), 1.0);
        for bad in [
            r#"{"gpu":{"capacity":0}}"#,
            r#"{"gpu":{"capacity":-1}}"#,
            r#"{"gpu":{}}"#,
            r#"{"gpu":{"capacity":1,"other":1}}"#,
        ] {
            assert!(graph(bad).is_err(), "{bad}");
        }
        let empty = serde_json::to_value(Graph::default()).unwrap();
        assert!(!empty.as_object().unwrap().contains_key("resources"));
    }
}
