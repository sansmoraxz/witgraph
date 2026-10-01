//! Validation diagnostics: errors as data, never panics.
//!
//! Each [`Diagnostic`] is a [`miette::Diagnostic`]: severity and stable codes
//! (`witgraph::ir::*`) come from the derive; graph coordinates stay
//! structural in [`Location`].

use core::fmt;

pub use miette::Severity;

use crate::id::{ComponentRef, ConnectionId, NodeId, PortName, PortRef};
use crate::port::{PortDirection, PortKind};

/// Where in the graph a diagnostic points.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Location {
    /// The graph as a whole.
    Graph,
    /// One component contract in the graph's component table.
    Component(ComponentRef),
    /// One node.
    Node(NodeId),
    /// One port on one node.
    Port(PortRef),
    /// One connection.
    Connection(ConnectionId),
    /// The nodes forming a cycle.
    Cycle(Vec<NodeId>),
}

fn join<T: fmt::Display>(items: &[T]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&item.to_string());
    }
    out
}

/// Like [`join`], but repeated items render once (first occurrence wins).
fn join_unique<T: fmt::Display + Eq>(items: &[T]) -> String {
    let mut seen: Vec<&T> = Vec::with_capacity(items.len());
    for item in items {
        if !seen.contains(&item) {
            seen.push(item);
        }
    }
    let mut out = String::new();
    for (i, item) in seen.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&item.to_string());
    }
    out
}

/// Component refs including their content hashes, which the `Display` impl
/// deliberately omits.
fn join_with_hashes(refs: &[ComponentRef]) -> String {
    let mut out = String::new();
    for (i, r) in refs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&r.to_string());
        if let Some(hash) = &r.content_hash {
            out.push('#');
            out.push_str(hash);
        }
    }
    out
}

/// Every defect graph validation can detect, as data. The `thiserror`
/// message is the user-facing rendering; severity comes from the
/// [`miette::Diagnostic`] derive (`None` = error, per miette convention).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Diagnostic {
    /// A node references a component missing from the graph's component
    /// table.
    #[error("node `{node}` references unknown component `{component}`")]
    #[diagnostic(code(witgraph::ir::unknown_component))]
    UnknownComponent {
        /// The referencing node.
        node: NodeId,
        /// The unresolved component reference.
        component: ComponentRef,
    },
    /// A component table entry for which the [`ContractSource`] supplied no
    /// contract — the component is unknown to it, or the source only has a
    /// revision with a different content hash.
    ///
    /// [`ContractSource`]: crate::ContractSource
    #[error("no contract was supplied for component `{0}`")]
    #[diagnostic(
        code(witgraph::ir::contract_not_found),
        help(
            "looked up {}; re-derive the contract from the component's WIT, or re-pin the graph to the revision you have",
            join_with_hashes(core::slice::from_ref(_0))
        )
    )]
    ContractNotFound(ComponentRef),
    /// Two entries in the graph's component table share an identity
    /// (package, world, version, and content hash all equal).
    #[error("component table contains duplicate component `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_component))]
    DuplicateComponent(ComponentRef),
    /// A contract declares two ports with the same name on the same side.
    #[error("component `{component}` declares duplicate {direction} port `{port}`")]
    #[diagnostic(code(witgraph::ir::duplicate_port_name))]
    DuplicatePortName {
        /// The component declaring the clashing ports.
        component: ComponentRef,
        /// Which side of the contract the clash is on.
        direction: PortDirection,
        /// The repeated port name.
        port: PortName,
    },
    /// A node's component reference matches more than one contract in the
    /// graph's component table (e.g. an unhashed reference amid several
    /// hashed revisions of the same world).
    #[error(
        "node `{node}` component reference `{component}` is ambiguous: it matches {} contracts",
        matches.len()
    )]
    #[diagnostic(
        code(witgraph::ir::ambiguous_component),
        help("matching contracts: {}", join_with_hashes(matches))
    )]
    AmbiguousComponent {
        /// The referencing node.
        node: NodeId,
        /// The ambiguous component reference.
        component: ComponentRef,
        /// Every contract id the reference matches.
        matches: Vec<ComponentRef>,
    },
    /// Two nodes share an id.
    #[error("duplicate node id `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_node_id))]
    DuplicateNodeId(NodeId),
    /// Two connections share an id.
    #[error("duplicate connection id `{0}`")]
    #[diagnostic(code(witgraph::ir::duplicate_connection_id))]
    DuplicateConnectionId(ConnectionId),
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
    /// An output port marked optional: optionality only applies to inputs.
    #[error(
        "output port `{port}` on `{component}` is marked optional; only inputs may be optional"
    )]
    #[diagnostic(code(witgraph::ir::optional_output))]
    OptionalOutput {
        /// The component declaring the flagged output.
        component: ComponentRef,
        /// The flagged output port.
        port: PortName,
    },
    /// An input port marked optional whose kind is Stream or Future. Only
    /// Value inputs can be optional: an unconnected value reads as `none`,
    /// but there is no handle to hand a node for an unconnected stream or
    /// future.
    #[error(
        "input port `{port}` on `{component}` is an optional {kind}; only value inputs may be optional"
    )]
    #[diagnostic(code(witgraph::ir::optional_async_input))]
    OptionalAsyncInput {
        /// The component declaring the flagged input.
        component: ComponentRef,
        /// The flagged input port.
        port: PortName,
        /// The port's kind (Stream or Future).
        kind: PortKind,
    },
    /// A Stream or Future output with more than one outgoing connection.
    /// Stream and future handles move to exactly one consumer; fan-out is
    /// the job of a dedicated tee node.
    #[error(
        "{kind} output `{port}` has {} consumers ({}); stream and future outputs connect to at most one input",
        connections.len(),
        join_unique(connections)
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
    /// Value can cross: a stream or future handle belongs to the run that
    /// created it.
    #[error(
        "feedback connection `{conn}` carries a {kind} port; feedback connections must carry values"
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
            Diagnostic::UnknownComponent { node, .. }
            | Diagnostic::AmbiguousComponent { node, .. } => Location::Node(node.clone()),
            Diagnostic::DuplicateNodeId(node) => Location::Node(node.clone()),
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
            Diagnostic::MultipleWriters { port, .. }
            | Diagnostic::AsyncFanOut { port, .. }
            | Diagnostic::RequiredInputUnconnected { port } => Location::Port(port.clone()),
            Diagnostic::DuplicateComponent(component) | Diagnostic::ContractNotFound(component) => {
                Location::Component(component.clone())
            }
            Diagnostic::DuplicatePortName { component, .. }
            | Diagnostic::OptionalOutput { component, .. }
            | Diagnostic::OptionalAsyncInput { component, .. } => {
                Location::Component(component.clone())
            }
            Diagnostic::IllegalCycle { nodes } => Location::Cycle(nodes.clone()),
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
    use crate::id::PackageRef;

    fn cref(hash: Option<&str>) -> ComponentRef {
        ComponentRef {
            package: PackageRef {
                namespace: "demo".into(),
                name: "graph".into(),
                version: None,
            },
            world: "w".into(),
            content_hash: hash.map(String::from),
        }
    }

    #[test]
    fn locations_point_at_the_offending_construct() {
        let cases: Vec<(Diagnostic, Location)> = vec![
            (
                Diagnostic::UnknownComponent {
                    node: "n".into(),
                    component: cref(None),
                },
                Location::Node("n".into()),
            ),
            (
                Diagnostic::DuplicateComponent(cref(None)),
                Location::Component(cref(None)),
            ),
            (
                Diagnostic::DuplicatePortName {
                    component: cref(None),
                    direction: PortDirection::Input,
                    port: "p".into(),
                },
                Location::Component(cref(None)),
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
    fn ambiguous_component_help_includes_hashes() {
        use miette::Diagnostic as _;
        let diagnostic = Diagnostic::AmbiguousComponent {
            node: "n".into(),
            component: cref(None),
            matches: vec![cref(Some("aa")), cref(Some("bb"))],
        };
        let help = diagnostic.help().expect("has help").to_string();
        assert_eq!(help, "matching contracts: demo:graph/w#aa, demo:graph/w#bb");
    }
}
