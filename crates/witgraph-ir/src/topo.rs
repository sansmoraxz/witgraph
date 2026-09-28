//! Topology analysis over `petgraph`: topological ordering and Tarjan SCCs.
//!
//! Every function here takes the set of `resolved` connection ids and
//! analyzes only those edges. The caller passes the connections whose
//! endpoints fully resolve (both nodes exist and both ports are declared);
//! everything else is excluded — its defect is reported separately by
//! validation, so unresolvable edges never distort cycle analysis. Node ids
//! must be unique (graph vertices are keyed by node id).

use std::collections::{BTreeSet, HashMap, HashSet};

use petgraph::algo::{tarjan_scc, toposort};
use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::Graph;
use crate::id::{ConnectionId, NodeId};

/// Node indices follow declaration order, so `ids[i]` names `NodeIndex(i)`.
fn build(
    graph: &Graph,
    include_feedback: bool,
    resolved: &HashSet<&ConnectionId>,
) -> (DiGraph<(), ()>, Vec<NodeId>) {
    let ids: Vec<NodeId> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let index: HashMap<&NodeId, NodeIndex> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id, NodeIndex::new(i)))
        .collect();
    let mut g = DiGraph::with_capacity(ids.len(), graph.connections.len());
    for _ in &ids {
        g.add_node(());
    }
    for conn in &graph.connections {
        if (conn.feedback && !include_feedback) || !resolved.contains(&conn.id) {
            continue;
        }
        if let (Some(&from), Some(&to)) = (index.get(&conn.from.node), index.get(&conn.to.node)) {
            g.add_edge(from, to, ());
        }
    }
    (g, ids)
}

/// Topological order over non-feedback edges, or the illegal cycles that
/// prevent one. `toposort` fails exactly when a non-feedback cycle exists,
/// so the two arms are exhaustive.
pub(crate) fn acyclic_order(
    graph: &Graph,
    resolved: &HashSet<&ConnectionId>,
) -> Result<Vec<NodeId>, Vec<Vec<NodeId>>> {
    let (g, ids) = build(graph, false, resolved);
    match toposort(&g, None) {
        Ok(order) => Ok(order.into_iter().map(|v| ids[v.index()].clone()).collect()),
        Err(_) => Err(illegal_cycles(&g, &ids)),
    }
}

/// Cycles that contain no feedback edge: SCCs of size > 1 over non-feedback
/// edges, plus non-feedback self-loops. Deterministic order for diagnostics.
fn illegal_cycles(g: &DiGraph<(), ()>, ids: &[NodeId]) -> Vec<Vec<NodeId>> {
    let mut cycles: Vec<Vec<usize>> = tarjan_scc(g)
        .into_iter()
        .map(|scc| scc.into_iter().map(NodeIndex::index).collect::<Vec<_>>())
        .filter(|scc| {
            scc.len() > 1 || g.contains_edge(NodeIndex::new(scc[0]), NodeIndex::new(scc[0]))
        })
        .collect();
    for scc in &mut cycles {
        scc.sort_unstable();
    }
    cycles.sort_unstable();
    cycles
        .into_iter()
        .map(|scc| scc.into_iter().map(|i| ids[i].clone()).collect())
        .collect()
}

/// SCC index per node over ALL edges (feedback included). An edge u→v lies
/// on some cycle iff u and v share an SCC.
pub(crate) fn scc_membership(
    graph: &Graph,
    resolved: &HashSet<&ConnectionId>,
) -> HashMap<NodeId, usize> {
    let (g, ids) = build(graph, true, resolved);
    let mut scc_of = HashMap::with_capacity(ids.len());
    for (scc_id, scc) in tarjan_scc(&g).into_iter().enumerate() {
        for v in scc {
            scc_of.insert(ids[v.index()].clone(), scc_id);
        }
    }
    scc_of
}

/// Feedback edges that participate in some cycle (over ALL edges). Feedback
/// edges not in this set earn a `UselessFeedback` warning.
pub(crate) fn useful_feedback_edges(
    graph: &Graph,
    resolved: &HashSet<&ConnectionId>,
) -> BTreeSet<ConnectionId> {
    let scc = scc_membership(graph, resolved);
    graph
        .connections
        .iter()
        .filter(|conn| {
            conn.feedback
                && scc
                    .get(&conn.from.node)
                    .is_some_and(|from| scc.get(&conn.to.node).is_some_and(|to| from == to))
        })
        .map(|conn| conn.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::PortRef;

    fn graph(nodes: &[&str], edges: &[(&str, &str, bool)]) -> Graph {
        let mut builder = Graph::builder("topo-test");
        for node in nodes {
            builder = builder.add_node(
                *node,
                "demo:graph/test".parse().expect("valid component ref"),
            );
        }
        for (i, (from, to, feedback)) in edges.iter().enumerate() {
            let id = format!("c{i}");
            let from = PortRef::new(*from, "out");
            let to = PortRef::new(*to, "in");
            builder = if *feedback {
                builder.connect_feedback(id, from, to)
            } else {
                builder.connect(id, from, to)
            };
        }
        builder.build()
    }

    fn names(order: &[NodeId]) -> Vec<&str> {
        order.iter().map(|id| id.as_str()).collect()
    }

    fn all(graph: &Graph) -> HashSet<&ConnectionId> {
        graph.connections.iter().map(|c| &c.id).collect()
    }

    #[test]
    fn diamond_order_is_deterministic() {
        let g = graph(
            &["d", "b", "a", "c"],
            &[
                ("a", "b", false),
                ("a", "c", false),
                ("b", "d", false),
                ("c", "d", false),
            ],
        );
        let order = acyclic_order(&g, &all(&g)).unwrap();
        assert_eq!(order.first().map(|id| id.as_str()), Some("a"));
        assert_eq!(order.last().map(|id| id.as_str()), Some("d"));
        // Any consistent order is acceptable; petgraph's toposort is
        // deterministic for a fixed insertion order.
        assert_eq!(acyclic_order(&g, &all(&g)).unwrap(), order);
    }

    #[test]
    fn cycle_without_feedback_is_illegal() {
        let g = graph(
            &["a", "b", "c"],
            &[("a", "b", false), ("b", "c", false), ("c", "a", false)],
        );
        let cycles = acyclic_order(&g, &all(&g)).unwrap_err();
        assert_eq!(cycles.len(), 1);
        assert_eq!(names(&cycles[0]), ["a", "b", "c"]);
    }

    #[test]
    fn feedback_edge_breaks_cycle() {
        let g = graph(
            &["a", "b", "c"],
            &[("a", "b", false), ("b", "c", false), ("c", "a", true)],
        );
        let order = acyclic_order(&g, &all(&g)).unwrap();
        assert_eq!(names(&order), ["a", "b", "c"]);
        assert_eq!(
            useful_feedback_edges(&g, &all(&g)),
            BTreeSet::from([ConnectionId::from("c2")])
        );
    }

    #[test]
    fn self_loops() {
        let plain = graph(&["a"], &[("a", "a", false)]);
        assert_eq!(
            acyclic_order(&plain, &all(&plain)).unwrap_err(),
            vec![vec![NodeId::from("a")]]
        );

        let feedback = graph(&["a"], &[("a", "a", true)]);
        assert!(acyclic_order(&feedback, &all(&feedback)).is_ok());
        assert_eq!(
            useful_feedback_edges(&feedback, &all(&feedback)),
            BTreeSet::from([ConnectionId::from("c0")])
        );
    }

    #[test]
    fn useless_feedback_on_acyclic_path() {
        let g = graph(&["a", "b"], &[("a", "b", true)]);
        assert!(useful_feedback_edges(&g, &all(&g)).is_empty());
        assert!(acyclic_order(&g, &all(&g)).is_ok());
    }

    #[test]
    fn scc_membership_groups_cycles_over_all_edges() {
        let g = graph(
            &["a", "b", "c", "d"],
            &[
                ("a", "b", false),
                ("b", "a", true),
                ("c", "c", true),
                ("a", "d", false),
            ],
        );
        let scc = scc_membership(&g, &all(&g));
        assert_eq!(scc[&NodeId::from("a")], scc[&NodeId::from("b")]);
        assert_ne!(scc[&NodeId::from("a")], scc[&NodeId::from("d")]);
        assert_ne!(scc[&NodeId::from("a")], scc[&NodeId::from("c")]);
    }

    #[test]
    fn unresolved_connections_never_form_cycles() {
        let g = graph(&["a", "b"], &[("a", "b", false), ("b", "a", false)]);
        assert!(acyclic_order(&g, &all(&g)).is_err());
        let only_first = HashSet::from([&g.connections[0].id]);
        assert!(acyclic_order(&g, &only_first).is_ok());
    }

    #[test]
    fn two_feedback_edges_one_cycle_each() {
        // a -> b -> a (feedback on return), plus feedback a -> c (no cycle).
        let g = graph(
            &["a", "b", "c"],
            &[("a", "b", false), ("b", "a", true), ("a", "c", true)],
        );
        assert_eq!(
            useful_feedback_edges(&g, &all(&g)),
            BTreeSet::from([ConnectionId::from("c1")])
        );
    }
}
