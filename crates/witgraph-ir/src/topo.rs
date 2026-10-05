//! Topology analysis over `petgraph`: topological ordering and strongly
//! connected components. Only iterative algorithms are used (Kahn's
//! algorithm, `kosaraju_scc`): graphs come from data, and a recursive
//! search would overflow the stack on a long enough chain.
//!
//! Every function here takes `resolved`, one flag per connection (by
//! position in `graph.connections`), and analyzes only the flagged edges.
//! The caller flags the connections whose endpoints fully resolve (both
//! nodes exist and both ports are declared); everything else is excluded —
//! its defect is reported separately by validation, so unresolvable edges
//! never distort cycle analysis, even when they share an id with a resolved
//! one. Graph vertices are keyed by node id; a duplicated id is one vertex
//! (its first declaration), so the analysis still runs on such a graph.

use std::collections::{BTreeSet, HashMap};

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use petgraph::algo::kosaraju_scc;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::Graph;
use crate::id::NodeId;

/// Node indices follow declaration order of each id's first node, so
/// `ids[i]` names `NodeIndex(i)`.
fn build(
    graph: &Graph,
    include_feedback: bool,
    resolved: &[bool],
) -> (DiGraph<(), ()>, Vec<NodeId>) {
    let mut ids: Vec<NodeId> = Vec::with_capacity(graph.nodes.len());
    let mut index: HashMap<&NodeId, NodeIndex> = HashMap::with_capacity(graph.nodes.len());
    for node in &graph.nodes {
        if !index.contains_key(&node.id) {
            index.insert(&node.id, NodeIndex::new(ids.len()));
            ids.push(node.id.clone());
        }
    }
    let mut g = DiGraph::with_capacity(ids.len(), graph.connections.len());
    for _ in &ids {
        g.add_node(());
    }
    for (conn, &ok) in graph.connections.iter().zip(resolved) {
        if (conn.feedback && !include_feedback) || !ok {
            continue;
        }
        if let (Some(&from), Some(&to)) = (index.get(&conn.from.node), index.get(&conn.to.node)) {
            g.add_edge(from, to, ());
        }
    }
    (g, ids)
}

/// Topological order over non-feedback edges, or the illegal cycles that
/// prevent one.
///
/// Kahn's algorithm, taking the earliest-declared ready node each step, so
/// the order is stable: the same graph always orders the same way, and a
/// graph with no edges keeps declaration order. (Independent nodes need not
/// keep their relative order in general: with `a, b, c` declared and an
/// edge `c -> a`, the order is `b, c, a`.) It leaves nodes unordered exactly
/// when a non-feedback cycle exists, so the two arms are exhaustive.
pub(crate) fn acyclic_order(
    graph: &Graph,
    resolved: &[bool],
) -> Result<Vec<NodeId>, Vec<Vec<NodeId>>> {
    let (g, ids) = build(graph, false, resolved);
    let mut indegree = vec![0usize; ids.len()];
    for edge in g.edge_indices() {
        if let Some((_, to)) = g.edge_endpoints(edge) {
            indegree[to.index()] += 1;
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = indegree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| Reverse(i))
        .collect();
    let mut order = Vec::with_capacity(ids.len());
    while let Some(Reverse(node)) = ready.pop() {
        order.push(ids[node].clone());
        for next in g.neighbors(NodeIndex::new(node)) {
            let d = &mut indegree[next.index()];
            *d -= 1;
            if *d == 0 {
                ready.push(Reverse(next.index()));
            }
        }
    }
    if order.len() == ids.len() {
        Ok(order)
    } else {
        Err(illegal_cycles(&g, &ids))
    }
}

/// Cycles that contain no feedback edge: SCCs of size > 1 over non-feedback
/// edges, plus non-feedback self-loops. Deterministic order for diagnostics.
fn illegal_cycles(g: &DiGraph<(), ()>, ids: &[NodeId]) -> Vec<Vec<NodeId>> {
    let cycles: Vec<Vec<usize>> = kosaraju_scc(g)
        .into_iter()
        .map(|scc| scc.into_iter().map(NodeIndex::index).collect::<Vec<_>>())
        .filter(|scc| {
            scc.len() > 1 || g.contains_edge(NodeIndex::new(scc[0]), NodeIndex::new(scc[0]))
        })
        .collect();
    let mut cycles: Vec<Vec<NodeId>> = cycles
        .into_iter()
        .map(|scc| {
            let mut nodes: Vec<NodeId> = scc.into_iter().map(|i| ids[i].clone()).collect();
            nodes.sort_unstable();
            nodes
        })
        .collect();
    cycles.sort_unstable();
    cycles
}

/// SCC index per node over ALL edges (feedback included). An edge u→v lies
/// on some cycle iff u and v share an SCC.
pub(crate) fn scc_membership(graph: &Graph, resolved: &[bool]) -> HashMap<NodeId, usize> {
    let (g, ids) = build(graph, true, resolved);
    let mut scc_of = HashMap::with_capacity(ids.len());
    for (scc_id, scc) in kosaraju_scc(&g).into_iter().enumerate() {
        for v in scc {
            scc_of.insert(ids[v.index()].clone(), scc_id);
        }
    }
    scc_of
}

/// Feedback edges (by connection position) that participate in some cycle
/// over ALL resolved edges.
pub(crate) fn useful_feedback_edges(graph: &Graph, resolved: &[bool]) -> BTreeSet<usize> {
    let scc = scc_membership(graph, resolved);
    graph
        .connections
        .iter()
        .enumerate()
        .filter(|(i, conn)| {
            conn.feedback
                && resolved.get(*i).copied().unwrap_or(false)
                && scc
                    .get(&conn.from.node)
                    .is_some_and(|from| scc.get(&conn.to.node).is_some_and(|to| from == to))
        })
        .map(|(i, _)| i)
        .collect()
}

/// The vertices of `0..count` that share a cycle over `edges`: every
/// strongly connected component with more than one vertex, sorted, in
/// order of its smallest vertex. Self-loops are ignored.
pub(crate) fn cyclic_groups(
    count: usize,
    edges: impl Iterator<Item = (usize, usize)>,
) -> Vec<Vec<usize>> {
    let mut g: DiGraph<(), ()> = DiGraph::with_capacity(count, 0);
    for _ in 0..count {
        g.add_node(());
    }
    for (from, to) in edges {
        if from != to {
            g.add_edge(NodeIndex::new(from), NodeIndex::new(to), ());
        }
    }
    let mut groups: Vec<Vec<usize>> = kosaraju_scc(&g)
        .into_iter()
        .filter(|scc| scc.len() > 1)
        .map(|scc| {
            let mut group: Vec<usize> = scc.into_iter().map(NodeIndex::index).collect();
            group.sort_unstable();
            group
        })
        .collect();
    groups.sort_unstable();
    groups
}

/// Longest-path depth from sources over non-feedback edges.
///
/// Nodes with no predecessors (through non-feedback resolved edges) get
/// depth 0. Every other node gets `max(depth of predecessors) + 1`.
/// The result is deterministic given a deterministic `order`.
pub(crate) fn depth_map(
    graph: &Graph,
    order: &[NodeId],
    resolved: &[bool],
) -> HashMap<NodeId, usize> {
    let mut preds: HashMap<&NodeId, Vec<&NodeId>> = HashMap::new();
    for (conn, &ok) in graph.connections.iter().zip(resolved) {
        if conn.feedback || !ok {
            continue;
        }
        preds
            .entry(&conn.to.node)
            .or_default()
            .push(&conn.from.node);
    }

    let mut depth: HashMap<NodeId, usize> = HashMap::with_capacity(order.len());
    for node_id in order {
        let d = preds
            .get(node_id)
            .map(|pred_nodes| {
                pred_nodes
                    .iter()
                    .filter_map(|p| depth.get(*p))
                    .max()
                    .map_or(0, |max_d| max_d + 1)
            })
            .unwrap_or(0);
        depth.insert(node_id.clone(), d);
    }
    depth
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

    fn all(graph: &Graph) -> Vec<bool> {
        vec![true; graph.connections.len()]
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
    fn depth_is_the_longest_path_over_non_feedback_edges() {
        // a -> b -> d, a -> d, c alone, d -> a feedback.
        let g = graph(
            &["a", "b", "c", "d"],
            &[
                ("a", "b", false),
                ("b", "d", false),
                ("a", "d", false),
                ("d", "a", true),
            ],
        );
        let order = acyclic_order(&g, &all(&g)).unwrap();
        let depth = depth_map(&g, &order, &all(&g));
        let at = |n: &str| depth[&NodeId::from(n)];
        assert_eq!((at("a"), at("b"), at("c"), at("d")), (0, 1, 0, 2));
        // An unresolved edge counts for nothing.
        let depth = depth_map(&g, &order, &[true, false, true, true]);
        assert_eq!(depth[&NodeId::from("d")], 1);
    }

    #[test]
    fn independent_nodes_keep_declaration_order() {
        let g = graph(&["a", "b", "c", "d"], &[("c", "d", false)]);
        let order = acyclic_order(&g, &all(&g)).unwrap();
        assert_eq!(names(&order), ["a", "b", "c", "d"]);
        let g = graph(&["d", "c", "b", "a"], &[("a", "d", false)]);
        let order = acyclic_order(&g, &all(&g)).unwrap();
        assert_eq!(
            names(&order),
            ["c", "b", "a", "d"],
            "edges first, then declaration"
        );
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
        assert_eq!(useful_feedback_edges(&g, &all(&g)), BTreeSet::from([2]));
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
            BTreeSet::from([0])
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
        assert!(acyclic_order(&g, &[true, false]).is_ok());
    }

    #[test]
    fn cyclic_groups_are_the_nontrivial_sccs() {
        // 0 -> 1 -> 2 -> 0, 3 -> 4 -> 3, 5 -> 5 (self-loop), 6 alone.
        let edges = [(0, 1), (1, 2), (2, 0), (4, 3), (3, 4), (5, 5), (2, 6)];
        assert_eq!(
            cyclic_groups(7, edges.into_iter()),
            vec![vec![0, 1, 2], vec![3, 4]]
        );
        assert!(cyclic_groups(3, [(0, 1), (1, 2)].into_iter()).is_empty());
    }

    #[test]
    fn two_feedback_edges_one_cycle_each() {
        // a -> b -> a (feedback on return), plus feedback a -> c (no cycle).
        let g = graph(
            &["a", "b", "c"],
            &[("a", "b", false), ("b", "a", true), ("a", "c", true)],
        );
        assert_eq!(useful_feedback_edges(&g, &all(&g)), BTreeSet::from([1]));
    }
}
