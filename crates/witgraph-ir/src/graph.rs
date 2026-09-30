//! The graph IR: nodes, connections, metadata, and the builder.
//!
//! A [`Graph`] is self-contained — it embeds the component contracts its
//! nodes reference, so a serialized graph is validatable and compilable with
//! no editor, filesystem, or WIT resolver present.

use std::collections::BTreeMap;

use crate::component::ComponentContract;
use crate::id::{ComponentRef, ConnectionId, NodeId, PortName, PortRef, ResourceId};
use crate::val::Val;

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
            return Err(format!(
                "fraction {value} is outside (0.0, 1.0]"
            ));
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
/// Each named resource pool has an implicit capacity of 1.0. The scheduler
/// defers activation when the sum of active claims on any resource would
/// exceed 1.0.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResourceClaim {
    /// Fraction of the resource consumed while active.
    pub fraction: Fraction,
    /// If true, retain allocation through suspension (e.g., VRAM).
    /// If false (default), release when WASM finishes (e.g., GPU compute).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub hold: bool,
}

impl ResourceClaim {
    /// A non-held claim (auto-released on suspension).
    pub fn new(fraction: f64) -> Result<Self, String> {
        Ok(Self {
            fraction: Fraction::new(fraction)?,
            hold: false,
        })
    }

    /// A held claim (retained through suspension).
    pub fn held(fraction: f64) -> Result<Self, String> {
        Ok(Self {
            fraction: Fraction::new(fraction)?,
            hold: true,
        })
    }
}

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
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Node {
    /// The node's graph-unique id.
    pub id: NodeId,
    /// The component this node instantiates.
    pub component: ComponentRef,
    /// Display label, if it differs from the id.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub label: Option<String>,
    /// Initial values for Value input ports, seeded before the first
    /// activation. Each key must name a Value-kind input port on the
    /// node's component; the value must match the port's declared type.
    /// Validated at compile time.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub config: BTreeMap<PortName, Val>,
    /// Fractional claims on named shared resources, consumed while the
    /// node is active. The scheduler defers activation when claims would
    /// exceed capacity. Validated at compile time.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub resources: BTreeMap<ResourceId, ResourceClaim>,
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
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// Merges a values snapshot into per-node config.
    ///
    /// For each entry whose node id exists in the graph, the
    /// corresponding `Node.config` values are merged (existing entries
    /// for the same port are overwritten; other ports are preserved).
    /// Unknown node ids are silently ignored — validation happens at
    /// [`Graph::compile`].
    pub fn apply_snapshot(
        &mut self,
        snapshot: &BTreeMap<NodeId, BTreeMap<PortName, Val>>,
    ) {
        for node in &mut self.nodes {
            if let Some(ports) = snapshot.get(&node.id) {
                for (port, val) in ports {
                    node.config.insert(port.clone(), val.clone());
                }
            }
        }
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
            config: BTreeMap::new(),
            resources: BTreeMap::new(),
        });
        self
    }

    /// Sets an initial value for a Value input port on the named node.
    ///
    /// The port must be a Value-kind input on the node's component;
    /// compile will reject mismatches. Overwrites any previous config
    /// for the same port.
    pub fn set_config(
        mut self,
        node: impl Into<NodeId>,
        port: impl Into<PortName>,
        val: Val,
    ) -> Result<Self, String> {
        let node_id = node.into();
        let n = self
            .graph
            .nodes
            .iter_mut()
            .find(|n| n.id == node_id)
            .ok_or_else(|| format!("set_config: unknown node `{node_id}`"))?;
        n.config.insert(port.into(), val);
        Ok(self)
    }

    /// Declares that the named node consumes `claim.fraction` of the
    /// shared resource `resource` while active. Overwrites any previous
    /// claim on the same resource for the same node.
    pub fn set_resource(
        mut self,
        node: impl Into<NodeId>,
        resource: impl Into<ResourceId>,
        claim: ResourceClaim,
    ) -> Result<Self, String> {
        let node_id = node.into();
        let n = self
            .graph
            .nodes
            .iter_mut()
            .find(|n| n.id == node_id)
            .ok_or_else(|| format!("set_resource: unknown node `{node_id}`"))?;
        n.resources.insert(resource.into(), claim);
        Ok(self)
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
            config: BTreeMap::new(),
            resources: BTreeMap::new(),
        };
        let json = serde_json::to_value(&node).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("config"),
            "empty config omitted from wire format"
        );
        assert!(
            !json.as_object().unwrap().contains_key("resources"),
            "empty resources omitted from wire format"
        );
    }

    #[test]
    fn config_round_trips_through_serde() {
        let component = ComponentContract {
            id: ComponentRef {
                package: PackageRef {
                    namespace: "demo".into(),
                    name: "graph".into(),
                    version: Some(semver::Version::new(0, 1, 0)),
                },
                world: "echo".into(),
                content_hash: None,
            },
            inputs: vec![PortDef::new("rate", PortKind::Value, Type::U32)],
            outputs: vec![],
            capabilities: vec![],
            type_names: vec![],
            docs: None,
        };
        let graph = Graph::builder("cfg")
            .add_component(component.clone())
            .add_node("n", component.id.clone())
            .set_config("n", "rate", Val::U32(10)).unwrap()
            .build();

        let json = serde_json::to_string_pretty(&graph).unwrap();
        let back: Graph = serde_json::from_str(&json).unwrap();
        assert_eq!(graph, back);
        assert_eq!(
            back.nodes[0].config.get(&PortName::from("rate")),
            Some(&Val::U32(10))
        );
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::port::{PortDef, PortKind};
    use crate::types::Type;

    #[test]
    fn apply_snapshot_merges_into_config() {
        let cref: ComponentRef = "demo:graph/echo@0.1.0".parse().unwrap();
        let component = ComponentContract {
            id: cref.clone(),
            inputs: vec![
                PortDef::new("rate", PortKind::Value, Type::U32),
                PortDef::new("gain", PortKind::Value, Type::F64),
            ],
            outputs: vec![],
            capabilities: vec![],
            type_names: vec![],
            docs: None,
        };
        let mut graph = Graph::builder("t")
            .add_component(component)
            .add_node("n", cref)
            .set_config("n", "rate", Val::U32(10)).unwrap()
            .build();

        let mut snap = BTreeMap::new();
        let mut ports = BTreeMap::new();
        ports.insert(PortName::from("rate"), Val::U32(42));
        ports.insert(PortName::from("gain"), Val::F64(1.5));
        snap.insert(NodeId::from("n"), ports);

        graph.apply_snapshot(&snap);

        assert_eq!(
            graph.nodes[0].config.get(&PortName::from("rate")),
            Some(&Val::U32(42)),
            "existing config overwritten"
        );
        assert_eq!(
            graph.nodes[0].config.get(&PortName::from("gain")),
            Some(&Val::F64(1.5)),
            "new config added"
        );
    }

    #[test]
    fn apply_snapshot_ignores_unknown_nodes() {
        let mut graph = Graph::builder("t").build();
        let mut snap = BTreeMap::new();
        let mut ports = BTreeMap::new();
        ports.insert(PortName::from("x"), Val::Bool(true));
        snap.insert(NodeId::from("ghost"), ports);

        graph.apply_snapshot(&snap);
        assert!(graph.nodes.is_empty());
    }
}
