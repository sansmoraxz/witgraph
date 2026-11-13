//! Validation diagnostics: errors as data, never panics.
//!
//! Each [`Diagnostic`] is a [`miette::Diagnostic`]: severity and stable codes
//! (`witgraph::ir::*`) come from the derive; graph coordinates stay
//! structural in [`Location`].

use core::fmt;

pub use miette::Severity;

use crate::id::{
    ComponentName, ConnectionId, ContractId, LinkId, NodeId, PortName, PortRef, ResourceId,
};
use crate::port::{PortDirection, PortKind};

/// Where in the graph a diagnostic points.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Location {
    /// The graph as a whole.
    Graph,
    /// One contract: a world, as one component of the table provides it.
    Component(ContractId),
    /// One node.
    Node(NodeId),
    /// Several nodes, in declaration order.
    Nodes(Vec<NodeId>),
    /// One port on one node.
    Port(PortRef),
    /// One connection.
    Connection(ConnectionId),
    /// One link.
    Link(LinkId),
    /// The nodes forming a cycle.
    Cycle(Vec<NodeId>),
    /// The members of one stream group, or of several on a cycle.
    Group(Vec<NodeId>),
}

/// The items joined with `, `.
fn join<T: fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    let mut out = String::new();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&item.to_string());
    }
    out
}

/// Node ids, the first [`LISTED`] of them.
fn join_nodes(nodes: &[NodeId]) -> String {
    let mut out = join(nodes.iter().take(LISTED).map(|n| format!("`{n}`")));
    if nodes.len() > LISTED {
        out.push_str(&format!(" and {} more", nodes.len() - LISTED));
    }
    out
}

/// Like [`join`], but repeated items render once (first occurrence wins).
fn join_unique<T: fmt::Display + Eq + core::hash::Hash>(items: &[T]) -> String {
    let mut seen = std::collections::HashSet::with_capacity(items.len());
    join(items.iter().filter(|item| seen.insert(*item)))
}

/// At most this many items of a long list are rendered.
const LISTED: usize = 16;

/// Every defect graph validation can detect, as data. The `thiserror`
/// message is the user-facing rendering; severity comes from the
/// [`miette::Diagnostic`] derive (`None` = error, per miette convention).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Diagnostic {
    /// A node names a component the graph's component table does not
    /// have.
    #[error("node `{node}` names component `{component}`, which is not in the component table")]
    #[diagnostic(
        code(witgraph::ir::unknown_component),
        help("add it to the component table, with where its node manifest is")
    )]
    UnknownComponent {
        /// The node.
        node: NodeId,
        /// The component it names.
        component: ComponentName,
    },
    /// A link names a provider the graph's component table does not have.
    #[error("link `{link}` names provider `{provider}`, which is not in the component table")]
    #[diagnostic(
        code(witgraph::ir::unknown_provider),
        help("add it to the component table, with where its node manifest is")
    )]
    UnknownProvider {
        /// The link.
        link: LinkId,
        /// The provider it names.
        provider: ComponentName,
    },
    /// A world nodes run that the [`ContractSource`] has no contract of, as
    /// the component they name provides it: the component does not provide
    /// that world (its node manifest does not list it), or was not found.
    ///
    /// [`ContractSource`]: crate::ContractSource
    #[error("component `{}` provides no world `{}`", .0.component, .0.world)]
    #[diagnostic(
        code(witgraph::ir::contract_not_found),
        help("run one of the worlds the component's node manifest lists")
    )]
    ContractNotFound(ContractId),
    /// A contract declares two ports with the same name on the same side.
    #[error("component `{component}` declares duplicate {direction} port `{port}`")]
    #[diagnostic(code(witgraph::ir::duplicate_port_name))]
    DuplicatePortName {
        /// The contract declaring the clashing ports.
        component: ContractId,
        /// Which side of the contract the clash is on.
        direction: PortDirection,
        /// The repeated port name.
        port: PortName,
    },
    /// Two nodes share an id.
    #[error("duplicate node id `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_node_id))]
    DuplicateNodeId(NodeId),
    /// Two connections share an id.
    #[error("duplicate connection id `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_connection_id))]
    DuplicateConnectionId(ConnectionId),
    /// Two links share an id.
    #[error("duplicate link id `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_link_id))]
    DuplicateLinkId(LinkId),
    /// A link names a node that does not exist.
    #[error("link `{link}` references unknown node `{node}`")]
    #[diagnostic(code(witgraph::ir::link_unknown_node))]
    LinkUnknownNode {
        /// The link.
        link: LinkId,
        /// The missing node.
        node: NodeId,
    },
    /// A link names an import its node's component does not have, or one
    /// that cannot be linked: only an interface import can (a bare function
    /// or a world resource cannot).
    #[error("link `{link}`: node `{node}` has no linkable import `{import}`")]
    #[diagnostic(code(witgraph::ir::unknown_import))]
    UnknownImport {
        /// The link.
        link: LinkId,
        /// The node.
        node: NodeId,
        /// The import the link names.
        import: String,
    },
    /// Two links satisfy the same import of one node, or two imports a
    /// component merges into one (semver-compatible versions of one
    /// interface).
    #[error(
        "link `{second}` links import `{import}` of node `{node}`, which `{first}` already links"
    )]
    #[diagnostic(code(witgraph::ir::import_linked_twice))]
    ImportLinkedTwice {
        /// The node.
        node: NodeId,
        /// The import.
        import: String,
        /// The link declared first.
        first: LinkId,
        /// The redundant later link.
        second: LinkId,
    },
    /// Two connections share the same endpoints.
    #[error("connection `{second}` duplicates the endpoints of `{first}`")]
    #[diagnostic(code(witgraph::ir::duplicate_connection))]
    DuplicateConnection {
        /// The connection declared first.
        first: ConnectionId,
        /// The redundant later connection.
        second: ConnectionId,
    },
    /// A connection endpoint names a node that does not exist.
    #[error("connection `{conn}` references unknown node `{node}`")]
    #[diagnostic(code(witgraph::ir::unknown_node))]
    UnknownNode {
        /// The connection with the dangling endpoint.
        conn: ConnectionId,
        /// The missing node.
        node: NodeId,
    },
    /// A connection endpoint names a port its component does not declare.
    #[error("connection `{conn}` references unknown port `{port}`")]
    #[diagnostic(code(witgraph::ir::unknown_port))]
    UnknownPort {
        /// The connection with the dangling endpoint.
        conn: ConnectionId,
        /// The missing port.
        port: PortRef,
    },
    /// A connection's source resolves to an input port.
    #[error("connection `{conn}` source `{port}` is not an output port")]
    #[diagnostic(code(witgraph::ir::not_an_output))]
    NotAnOutput {
        /// The misdirected connection.
        conn: ConnectionId,
        /// The port used as a source.
        port: PortRef,
    },
    /// A connection's target resolves to an output port.
    #[error("connection `{conn}` target `{port}` is not an input port")]
    #[diagnostic(code(witgraph::ir::not_an_input))]
    NotAnInput {
        /// The misdirected connection.
        conn: ConnectionId,
        /// The port used as a target.
        port: PortRef,
    },
    /// A connection joins ports of different kinds (e.g. Value to Stream).
    #[error("connection `{conn}` mixes port kinds: {from} -> {to}")]
    #[diagnostic(code(witgraph::ir::kind_mismatch))]
    KindMismatch {
        /// The offending connection.
        conn: ConnectionId,
        /// Kind of the source port.
        from: PortKind,
        /// Kind of the target port.
        to: PortKind,
    },
    /// A connection joins ports whose payload types differ structurally.
    /// Reported only when the kinds already match — a kind mismatch on the
    /// same connection suppresses this check.
    #[error("connection `{conn}` mixes payload types: {from} -> {to}")]
    #[diagnostic(code(witgraph::ir::type_mismatch))]
    TypeMismatch {
        /// The offending connection.
        conn: ConnectionId,
        /// Rendered payload type of the source port.
        from: String,
        /// Rendered payload type of the target port.
        to: String,
    },
    /// An input port has more than one incoming connection.
    #[error(
        "input port `{port}` has multiple writers: {}",
        join_unique(connections)
    )]
    #[diagnostic(code(witgraph::ir::multiple_writers))]
    MultipleWriters {
        /// The over-written input port.
        port: PortRef,
        /// Every connection writing to it.
        connections: Vec<ConnectionId>,
    },
    /// A non-optional input port has no incoming connection.
    #[error("required input port `{port}` is unconnected")]
    #[diagnostic(code(witgraph::ir::required_input_unconnected))]
    RequiredInputUnconnected {
        /// The unconnected input port.
        port: PortRef,
    },
    /// A required input written only by feedback connections. A feedback
    /// edge delivers the previous iteration's value, so the input has no
    /// value (and its node cannot run) until the feedback source first
    /// produces one, or a value is injected.
    #[error(
        "required input port `{port}` is fed only by feedback connection `{conn}`; it has no value until its feedback source first produces one or a value is injected"
    )]
    #[diagnostic(code(witgraph::ir::feedback_only_input), severity(Warning))]
    FeedbackOnlyInput {
        /// The input port.
        port: PortRef,
        /// A feedback connection writing it.
        conn: ConnectionId,
    },
    /// An output port marked optional: optionality only applies to inputs.
    #[error(
        "output port `{port}` on `{component}` is marked optional; only inputs may be optional"
    )]
    #[diagnostic(code(witgraph::ir::optional_output))]
    OptionalOutput {
        /// The contract declaring the flagged output.
        component: ContractId,
        /// The flagged output port.
        port: PortName,
    },
    /// A Stream or Future output with more than one outgoing connection.
    /// Stream and future handles move to exactly one consumer; fan-out is
    /// the job of a dedicated tee node.
    #[error(
        "{kind} output `{port}` has {} consumers ({}); stream and future outputs connect to at most one input",
        connections.len(),
        join(connections)
    )]
    #[diagnostic(
        code(witgraph::ir::async_fan_out),
        help("insert a tee node to duplicate the {kind}")
    )]
    AsyncFanOut {
        /// The over-shared output port.
        port: PortRef,
        /// The port's kind (Stream or Future).
        kind: PortKind,
        /// Every connection reading from it.
        connections: Vec<ConnectionId>,
    },
    /// A node that claims more of a resource than there is, so it could never start.
    #[error("node `{node}` claims more than all of resource `{resource}`")]
    #[diagnostic(
        code(witgraph::ir::node_overclaims),
        help("lower its claim, or raise the resource's capacity")
    )]
    NodeOverclaims {
        /// The node.
        node: NodeId,
        /// The overclaimed resource.
        resource: ResourceId,
    },
    /// Stream groups, and the nodes between them, that wait on each other to
    /// start: a group starts only once every member's required inputs have
    /// values, and these are written only after it has run.
    #[error(
        "nodes {} can never start: a stream group among them needs a required input that is only written after it has run",
        join_nodes(nodes)
    )]
    #[diagnostic(
        code(witgraph::ir::group_waits_on_itself),
        help("make an input on the path optional, or mark a connection on it `feedback`")
    )]
    GroupWaitsOnItself {
        /// The nodes waiting on each other, sorted.
        nodes: Vec<NodeId>,
    },
    /// A cycle in which no edge is marked as a feedback boundary. `nodes`
    /// holds the members of the offending strongly connected component,
    /// sorted — not in traversal order.
    #[error("nodes {} form a cycle with no feedback boundary", join(nodes))]
    #[diagnostic(code(witgraph::ir::illegal_cycle))]
    IllegalCycle {
        /// The nodes forming the cycle, sorted.
        nodes: Vec<NodeId>,
    },
    /// A feedback-marked connection that lies on no cycle.
    #[error("feedback connection `{conn}` is not part of any cycle")]
    #[diagnostic(code(witgraph::ir::useless_feedback), severity(Warning))]
    UselessFeedback {
        /// The pointless feedback connection.
        conn: ConnectionId,
    },
    /// A feedback connection carries a Stream or Future port. A feedback
    /// edge is a unit-delay boundary between generations, which only a
    /// Value or a handle can cross: a stream or future belongs to the run
    /// that created it.
    #[error(
        "feedback connection `{conn}` carries a {kind} port; feedback connections must carry values or handles"
    )]
    #[diagnostic(code(witgraph::ir::async_feedback))]
    AsyncFeedback {
        /// The contradictory feedback connection.
        conn: ConnectionId,
        /// The source port.
        port: PortRef,
        /// The port's kind (Stream or Future).
        kind: PortKind,
    },
}

impl Diagnostic {
    /// Whether this defect rejects the graph. Anything not explicitly
    /// downgraded to a warning or advice is an error (miette's `None`
    /// severity means error by convention).
    pub fn is_error(&self) -> bool {
        use miette::Diagnostic as _;
        !matches!(self.severity(), Some(Severity::Warning | Severity::Advice))
    }

    /// Where in the graph this defect points.
    pub fn location(&self) -> Location {
        match self {
            Diagnostic::UnknownComponent { node, .. } => Location::Node(node.clone()),
            Diagnostic::DuplicateNodeId(node) => Location::Node(node.clone()),
            Diagnostic::NodeOverclaims { node, .. } => Location::Node(node.clone()),
            Diagnostic::DuplicateConnectionId(conn)
            | Diagnostic::DuplicateConnection { second: conn, .. }
            | Diagnostic::UnknownNode { conn, .. }
            | Diagnostic::UnknownPort { conn, .. }
            | Diagnostic::NotAnOutput { conn, .. }
            | Diagnostic::NotAnInput { conn, .. }
            | Diagnostic::KindMismatch { conn, .. }
            | Diagnostic::TypeMismatch { conn, .. }
            | Diagnostic::AsyncFeedback { conn, .. }
            | Diagnostic::UselessFeedback { conn } => Location::Connection(conn.clone()),
            Diagnostic::DuplicateLinkId(link)
            | Diagnostic::LinkUnknownNode { link, .. }
            | Diagnostic::UnknownProvider { link, .. }
            | Diagnostic::UnknownImport { link, .. }
            | Diagnostic::ImportLinkedTwice { second: link, .. } => Location::Link(link.clone()),
            Diagnostic::MultipleWriters { port, .. }
            | Diagnostic::AsyncFanOut { port, .. }
            | Diagnostic::RequiredInputUnconnected { port }
            | Diagnostic::FeedbackOnlyInput { port, .. } => Location::Port(port.clone()),
            Diagnostic::ContractNotFound(component)
            | Diagnostic::DuplicatePortName { component, .. }
            | Diagnostic::OptionalOutput { component, .. } => {
                Location::Component(component.clone())
            }
            Diagnostic::IllegalCycle { nodes } => Location::Cycle(nodes.clone()),
            Diagnostic::GroupWaitsOnItself { nodes } => Location::Group(nodes.clone()),
        }
    }
}

/// The diagnostics collected by one validation pass, in discovery order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Diagnostics(Vec<Diagnostic>);

impl Diagnostics {
    /// Records a defect.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.0.push(diagnostic);
    }

    /// Consumes the collection, yielding the diagnostics in discovery order.
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.0
    }

    /// True if any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.0.iter().any(Diagnostic::is_error)
    }

    /// True if no diagnostics were collected.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of diagnostics collected.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// All diagnostics, in discovery order.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter()
    }

    /// Only the error-severity diagnostics.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter().filter(|d| d.is_error())
    }

    /// Only the warning-severity diagnostics.
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter().filter(|d| !d.is_error())
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, diagnostic) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let severity = if diagnostic.is_error() {
                "error"
            } else {
                "warning"
            };
            write!(f, "{severity}: {diagnostic}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> ContractId {
        "c/demo:graph/w".parse().unwrap()
    }

    #[test]
    fn locations_point_at_the_offending_construct() {
        let cases: Vec<(Diagnostic, Location)> = vec![
            (
                Diagnostic::UnknownComponent {
                    node: "n".into(),
                    component: "c".parse().unwrap(),
                },
                Location::Node("n".into()),
            ),
            (
                Diagnostic::UnknownProvider {
                    link: "l".into(),
                    provider: "c".parse().unwrap(),
                },
                Location::Link("l".into()),
            ),
            (
                Diagnostic::ContractNotFound(contract()),
                Location::Component(contract()),
            ),
            (
                Diagnostic::DuplicatePortName {
                    component: contract(),
                    direction: PortDirection::Input,
                    port: "p".into(),
                },
                Location::Component(contract()),
            ),
            (
                Diagnostic::KindMismatch {
                    conn: "c".into(),
                    from: PortKind::Value,
                    to: PortKind::Stream,
                },
                Location::Connection("c".into()),
            ),
            (
                Diagnostic::AsyncFeedback {
                    conn: "c".into(),
                    port: PortRef::new("n", "p"),
                    kind: PortKind::Stream,
                },
                Location::Connection("c".into()),
            ),
            (
                Diagnostic::AsyncFanOut {
                    port: PortRef::new("n", "p"),
                    kind: PortKind::Stream,
                    connections: vec!["c1".into(), "c2".into()],
                },
                Location::Port(PortRef::new("n", "p")),
            ),
            (
                Diagnostic::RequiredInputUnconnected {
                    port: PortRef::new("n", "p"),
                },
                Location::Port(PortRef::new("n", "p")),
            ),
            (
                Diagnostic::IllegalCycle {
                    nodes: vec!["a".into(), "b".into()],
                },
                Location::Cycle(vec!["a".into(), "b".into()]),
            ),
        ];
        for (diagnostic, location) in cases {
            assert_eq!(diagnostic.location(), location, "{diagnostic}");
        }
    }

    #[test]
    fn multiple_writers_message_dedupes_ids() {
        let diagnostic = Diagnostic::MultipleWriters {
            port: PortRef::new("n", "in"),
            connections: vec!["c".into(), "c".into()],
        };
        assert_eq!(
            diagnostic.to_string(),
            "input port `n.in` has multiple writers: c"
        );
    }

    #[test]
    fn async_fan_out_message_counts_consumers() {
        let diagnostic = Diagnostic::AsyncFanOut {
            port: PortRef::new("n", "out"),
            kind: PortKind::Stream,
            connections: vec!["c1".into(), "c2".into()],
        };
        assert_eq!(
            diagnostic.to_string(),
            "stream output `n.out` has 2 consumers (c1, c2); stream and future outputs connect to at most one input"
        );
    }

    #[test]
    fn missing_contracts_name_the_component_and_world() {
        assert_eq!(
            Diagnostic::ContractNotFound(contract()).to_string(),
            "component `c` provides no world `demo:graph/w`"
        );
    }
}
