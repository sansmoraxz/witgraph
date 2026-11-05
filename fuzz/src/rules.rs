//! What a graph's compile should say, worked out from the documented rules with none of
//! the compiler's code: its own resolution, cycle search, groups and arithmetic.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use witgraph_ir::{Connection, Diagnostic, Graph};

use crate::catalog::{COMPONENTS, Component, Kind, Port};

/// One thing compiling should say, or did: what kind, and about what.
pub type Said = (&'static str, String);

/// A graph in the rules' terms. `None` when it uses what the rules don't model.
pub struct Model<'g> {
    graph: &'g Graph,
    /// By node id: the node's position and component. A repeated id means its first.
    nodes: BTreeMap<&'g str, (usize, &'static Component)>,
}

/// What a connection's ends turned out to be.
struct Ends {
    from: Option<(usize, &'static Port)>,
    to: Option<(usize, &'static Port)>,
    from_node: Option<usize>,
}

impl<'g> Model<'g> {
    /// The graph, if every component it uses is in the catalog, listed once, and it has
    /// no links: what's left is what the rules cover.
    pub fn new(graph: &'g Graph) -> Option<Self> {
        if !graph.links.is_empty() {
            return None;
        }
        let known = |r: &witgraph_ir::ComponentRef| COMPONENTS.iter().find(|c| c.reference() == *r);
        let mut listed = BTreeSet::new();
        for reference in &graph.components {
            known(reference)?;
            if !listed.insert(reference.to_string()) {
                return None;
            }
        }
        let mut nodes = BTreeMap::new();
        for (at, node) in graph.nodes.iter().enumerate() {
            if !graph.components.contains(&node.component) {
                return None;
            }
            nodes
                .entry(node.id.as_str())
                .or_insert((at, known(&node.component)?));
        }
        Some(Self { graph, nodes })
    }

    fn ends(&self, connection: &Connection) -> Ends {
        let from_node = self.nodes.get(connection.from.node.as_str());
        let to_node = self.nodes.get(connection.to.node.as_str());
        Ends {
            from: from_node.and_then(|(at, c)| {
                c.output(connection.from.port.as_str())
                    .map(|port| (*at, port))
            }),
            to: to_node
                .and_then(|(at, c)| c.input(connection.to.port.as_str()).map(|port| (*at, port))),
            from_node: from_node.map(|(at, _)| *at),
        }
    }

    /// What should be said of each connection and port, whatever else is wrong: errors
    /// and warnings together, each kind in its own name.
    pub fn local(&self) -> BTreeSet<Said> {
        let mut said = BTreeSet::new();
        let mut note = |kind: &'static str, about: String| {
            said.insert((kind, about));
        };

        let mut ids = BTreeSet::new();
        for node in &self.graph.nodes {
            if !ids.insert(node.id.as_str()) {
                note("duplicate_node_id", node.id.to_string());
            }
        }

        let mut connection_ids = BTreeSet::new();
        let mut pairs = BTreeSet::new();
        let mut writers: BTreeMap<String, Vec<&Connection>> = BTreeMap::new();
        let mut readers: BTreeMap<String, usize> = BTreeMap::new();
        for connection in &self.graph.connections {
            let id = connection.id.to_string();
            if !connection_ids.insert(id.clone()) {
                note("duplicate_connection_id", id.clone());
            }
            // (the same endpoints, feedback or not, is two writers anyway)
            if !pairs.insert((
                connection.from.to_string(),
                connection.to.to_string(),
                connection.feedback,
            )) {
                note("duplicate_connection", id.clone());
            }
            for (end, port, output) in [
                (&connection.from, &connection.from.port, true),
                (&connection.to, &connection.to.port, false),
            ] {
                match self.nodes.get(end.node.as_str()) {
                    None => note("unknown_node", format!("{id}:{}", end.node)),
                    Some((_, component)) => {
                        let (mine, other) = if output {
                            (
                                component.output(port.as_str()),
                                component.input(port.as_str()),
                            )
                        } else {
                            (
                                component.input(port.as_str()),
                                component.output(port.as_str()),
                            )
                        };
                        match (mine, other) {
                            (Some(_), _) => {}
                            (None, Some(_)) if output => note("not_an_output", id.clone()),
                            (None, Some(_)) => note("not_an_input", id.clone()),
                            (None, None) => note("unknown_port", format!("{id}:{end}")),
                        }
                    }
                }
            }
            let ends = self.ends(connection);
            if let (Some((_, from)), Some((_, to))) = (ends.from, ends.to) {
                if from.kind != to.kind {
                    note("kind_mismatch", id.clone());
                } else if !from.payload.fits(to) {
                    note("type_mismatch", id.clone());
                }
            }
            if connection.feedback
                && let Some((_, from)) = ends.from
                && from.kind.moves()
            {
                note("async_feedback", id.clone());
            }
            if ends.from.is_some() && ends.to.is_some() {
                *readers.entry(connection.from.to_string()).or_default() += 1;
            }
            // A connection from no node can never deliver.
            if ends.to.is_some() && ends.from_node.is_some() {
                writers
                    .entry(connection.to.to_string())
                    .or_default()
                    .push(connection);
            }
        }

        for (port, count) in &readers {
            let moves = self.graph.connections.iter().any(|c| {
                c.from.to_string() == *port
                    && self.ends(c).from.is_some_and(|(_, from)| from.kind.moves())
            });
            if moves && *count > 1 {
                note("async_fan_out", port.clone());
            }
        }
        for (port, connected) in &writers {
            if connected.len() > 1 {
                note("multiple_writers", port.clone());
            }
        }

        for (&id, (_, component)) in &self.nodes {
            for input in component.inputs {
                if input.optional {
                    continue;
                }
                let port = format!("{id}.{}", input.name);
                let real = writers.get(&port).map_or(&[][..], Vec::as_slice);
                if real.is_empty() {
                    note("required_input_unconnected", port);
                } else if !input.kind.moves() && real.iter().all(|c| c.feedback) {
                    note("feedback_only_input", port);
                }
            }
        }
        said
    }

    /// Whether the local rules found an error: past that, the compiler's global analysis
    /// sees a graph with parts missing, and says less.
    pub fn has_local_errors(local: &BTreeSet<Said>) -> bool {
        local.iter().any(|(kind, _)| *kind != "feedback_only_input")
    }

    /// `(position, position, connection)` of every connection, for a graph with no
    /// local errors.
    fn edges(&self) -> Vec<(usize, usize, &'g Connection)> {
        self.graph
            .connections
            .iter()
            .filter_map(|c| {
                let from = self.nodes.get(c.from.node.as_str())?.0;
                let to = self.nodes.get(c.to.node.as_str())?.0;
                Some((from, to, c))
            })
            .collect()
    }

    /// Strongly connected sets of nodes over non-feedback connections, by reachability.
    fn cycles(&self) -> BTreeSet<Vec<usize>> {
        let n = self.graph.nodes.len();
        let mut reach = vec![vec![false; n]; n];
        for (from, to, connection) in self.edges() {
            if !connection.feedback {
                reach[from][to] = true;
            }
        }
        close(&mut reach);
        (0..n)
            .filter(|&node| reach[node][node])
            .map(|node| {
                (0..n)
                    .filter(|&other| reach[node][other] && reach[other][node])
                    .collect()
            })
            .collect()
    }

    /// What should be said of the whole graph, once it has no local errors.
    pub fn global(&self) -> BTreeSet<Said> {
        let mut said = BTreeSet::new();
        let name = |at: usize| self.graph.nodes[at].id.to_string();
        let cycles = self.cycles();
        for cycle in &cycles {
            let mut names: Vec<String> = cycle.iter().map(|at| name(*at)).collect();
            names.sort();
            said.insert(("illegal_cycle", names.join(",")));
        }
        if !cycles.is_empty() {
            return said;
        }

        // A feedback connection lies on a cycle if its target reaches its source
        // over any connections at all.
        let n = self.graph.nodes.len();
        let edges = self.edges();
        let mut reach = vec![vec![false; n]; n];
        for (from, to, _) in &edges {
            reach[*from][*to] = true;
        }
        close(&mut reach);
        for (from, to, connection) in &edges {
            if connection.feedback && !(from == to || reach[*to][*from]) {
                said.insert(("useless_feedback", connection.id.to_string()));
            }
        }

        let groups = self.groups();
        for (at, node) in self.graph.nodes.iter().enumerate() {
            if groups.iter().any(|group| group.contains(&at)) {
                continue;
            }
            for (resource, claim) in &node.resources {
                let thousandths = (claim.amount.get() * 1000.0).round() as u64;
                if thousandths > self.capacity_thousandths(resource.as_str()) {
                    said.insert(("node_overclaims", resource.to_string()));
                }
            }
        }
        for group in &groups {
            let mut claims: BTreeMap<&str, u64> = BTreeMap::new();
            for at in group {
                for (resource, claim) in &self.graph.nodes[*at].resources {
                    *claims.entry(resource.as_str()).or_default() +=
                        (claim.amount.get() * 1000.0).round() as u64;
                }
            }
            for (resource, thousandths) in claims {
                if thousandths > self.capacity_thousandths(resource) {
                    said.insert(("group_overclaims", resource.to_string()));
                }
            }
        }
        if !self.can_all_start(&groups) {
            said.insert(("group_waits_on_itself", String::new()));
        }
        said
    }

    /// How much there is of a resource, in the thousandths claims are counted in.
    fn capacity_thousandths(&self, resource: &str) -> u64 {
        (self.graph.capacity_of(&resource.into()) * 1000.0).round() as u64
    }

    /// Nodes joined by stream and future connections, as sets of more than one node.
    pub fn groups(&self) -> Vec<Vec<usize>> {
        let n = self.graph.nodes.len();
        let mut label: Vec<usize> = (0..n).collect();
        loop {
            let mut changed = false;
            for (from, to, connection) in self.edges() {
                let moves = self
                    .ends(connection)
                    .from
                    .is_some_and(|(_, p)| p.kind.moves());
                if moves && label[from] != label[to] {
                    let low = label[from].min(label[to]);
                    let high = label[from].max(label[to]);
                    for slot in &mut label {
                        if *slot == high {
                            *slot = low;
                        }
                    }
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut sets: HashMap<usize, Vec<usize>> = HashMap::new();
        for (at, set) in label.iter().enumerate() {
            sets.entry(*set).or_default().push(at);
        }
        let mut groups: Vec<Vec<usize>> = sets.into_values().filter(|set| set.len() > 1).collect();
        groups.sort();
        groups
    }

    /// Whether every group and lone node can start: each starts once all its members'
    /// required inputs have values, and those written from outside it by a non-feedback
    /// connection have one only after their writer has started.
    fn can_all_start(&self, groups: &[Vec<usize>]) -> bool {
        let n = self.graph.nodes.len();
        let mut unit: Vec<usize> = (0..n).collect();
        for group in groups {
            for at in group {
                unit[*at] = group[0];
            }
        }
        let mut waits: Vec<(usize, usize)> = Vec::new();
        for (from, to, connection) in self.edges() {
            let required = self
                .ends(connection)
                .to
                .is_some_and(|(_, port)| !port.optional);
            let value = self
                .ends(connection)
                .from
                .is_some_and(|(_, port)| !matches!(port.kind, Kind::Stream | Kind::Future));
            // What is written inside its group does not hold the group up.
            if required && value && !connection.feedback && unit[to] != unit[from] {
                waits.push((unit[to], unit[from]));
            }
        }
        let mut started = vec![false; n];
        loop {
            let mut changed = false;
            for &u in &unit {
                if !started[u] && waits.iter().all(|(w, on)| *w != u || started[*on]) {
                    started[u] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        (0..n).all(|at| started[unit[at]])
    }
}

/// Adds to `reach` everything reachable through what it already has.
fn close(reach: &mut [Vec<bool>]) {
    for via in 0..reach.len() {
        let through = reach[via].clone();
        for row in reach.iter_mut() {
            if row[via] {
                for (slot, onward) in row.iter_mut().zip(&through) {
                    *slot |= *onward;
                }
            }
        }
    }
}

/// What compiling said, in the rules' terms. Diagnostics the rules don't speak of are
/// left out, and `allowed` says which those are.
pub fn observed(diagnostic: &Diagnostic) -> Option<Said> {
    use Diagnostic as D;
    Some(match diagnostic {
        D::DuplicateNodeId(node) => ("duplicate_node_id", node.to_string()),
        D::DuplicateConnectionId(id) => ("duplicate_connection_id", id.to_string()),
        D::DuplicateConnection { second, .. } => ("duplicate_connection", second.to_string()),
        D::UnknownNode { conn, node } => ("unknown_node", format!("{conn}:{node}")),
        D::UnknownPort { conn, port } => ("unknown_port", format!("{conn}:{port}")),
        D::NotAnOutput { conn, .. } => ("not_an_output", conn.to_string()),
        D::NotAnInput { conn, .. } => ("not_an_input", conn.to_string()),
        D::KindMismatch { conn, .. } => ("kind_mismatch", conn.to_string()),
        D::TypeMismatch { conn, .. } => ("type_mismatch", conn.to_string()),
        D::MultipleWriters { port, .. } => ("multiple_writers", port.to_string()),
        D::RequiredInputUnconnected { port } => ("required_input_unconnected", port.to_string()),
        D::FeedbackOnlyInput { port, .. } => ("feedback_only_input", port.to_string()),
        D::AsyncFanOut { port, .. } => ("async_fan_out", port.to_string()),
        D::AsyncFeedback { conn, .. } => ("async_feedback", conn.to_string()),
        D::IllegalCycle { nodes } => (
            "illegal_cycle",
            nodes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
        D::UselessFeedback { conn } => ("useless_feedback", conn.to_string()),
        D::NodeOverclaims { resource, .. } => ("node_overclaims", resource.to_string()),
        D::GroupOverclaims { resource, .. } => ("group_overclaims", resource.to_string()),
        D::GroupWaitsOnItself { .. } => ("group_waits_on_itself", String::new()),
        _ => return None,
    })
}

/// The kinds the local rules speak of, against those of the whole graph.
pub fn is_local(kind: &str) -> bool {
    !matches!(
        kind,
        "illegal_cycle"
            | "useless_feedback"
            | "group_overclaims"
            | "node_overclaims"
            | "group_waits_on_itself"
    )
}
