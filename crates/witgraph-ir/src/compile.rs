//! Graph compilation: the `Graph → CompiledGraph` typestate transition.
//!
//! Compilation validates the graph, collecting ALL diagnostics (never fail-fast, never panics).
//! Checks whose preconditions failed on a connection (unresolvable endpoint)
//! are suppressed on that connection rather than cascading noise.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Deref;

use crate::component::{Capability, ComponentContract};
use crate::diagnostics::{Diagnostic, Diagnostics};
use crate::graph::Graph;
use crate::id::{ComponentRef, ConnectionId, NodeId, PortName, PortRef};
use crate::port::{PortDef, PortDirection, PortKind};
use crate::topo;

/// A rejected graph, handed back for correction alongside everything wrong
/// with it.
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
#[error("graph compilation failed with {} error(s)", diagnostics.errors().count())]
#[diagnostic(code(witgraph::ir::compilation_failed))]
pub struct CompilationFailure {
    /// The rejected graph, returned so the caller can correct and retry.
    /// Boxed to keep the `Err` variant small.
    pub graph: Box<Graph>,
    /// Everything wrong with it (and any warnings found along the way).
    #[related]
    pub diagnostics: Diagnostics,
}

/// A graph that compiled successfully.
///
/// Sealed — no public constructor, private fields, not deserializable. The
/// only way in is [`Graph::compile`]; the way back out (for mutation) is
/// [`CompiledGraph::into_graph`].
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    graph: Graph,
    node_contract: HashMap<NodeId, usize>,
    order: Vec<NodeId>,
    warnings: Diagnostics,
}

impl CompiledGraph {
    /// Read access to the underlying graph.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Demote for mutation; the result must be re-compiled.
    pub fn into_graph(self) -> Graph {
        self.graph
    }

    /// Non-fatal diagnostics (e.g. `UselessFeedback`).
    pub fn warnings(&self) -> &Diagnostics {
        &self.warnings
    }

    /// Execution order over non-feedback edges.
    pub fn topological_order(&self) -> &[NodeId] {
        &self.order
    }

    /// The contract the given node instantiates.
    pub fn contract_for(&self, node: &NodeId) -> Option<&ComponentContract> {
        self.node_contract
            .get(node)
            .map(|&i| &self.graph.components[i])
    }

    /// Union of the capabilities of every component instantiated by a node,
    /// in deterministic order.
    pub fn required_capabilities(&self) -> BTreeSet<Capability> {
        self.graph
            .nodes
            .iter()
            .filter_map(|node| self.contract_for(&node.id))
            .flat_map(|contract| contract.capabilities.iter().cloned())
            .collect()
    }
}

impl Deref for CompiledGraph {
    type Target = Graph;

    fn deref(&self) -> &Graph {
        &self.graph
    }
}

fn resolve_endpoint<'g>(
    graph: &'g Graph,
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, usize>,
    conn: &ConnectionId,
    port_ref: &PortRef,
    direction: PortDirection,
    diagnostics: &mut Diagnostics,
) -> Option<&'g PortDef> {
    if !node_ids.contains(&port_ref.node) {
        diagnostics.push(Diagnostic::UnknownNode {
            conn: conn.clone(),
            node: port_ref.node.clone(),
        });
        return None;
    }
    // A known node with an unknown component was already diagnosed
    // (`UnknownComponent`); suppress dependent checks.
    let contract = &graph.components[*contracts.get(&port_ref.node)?];
    let (expected, opposite) = match direction {
        PortDirection::Output => (&contract.outputs, &contract.inputs),
        PortDirection::Input => (&contract.inputs, &contract.outputs),
    };
    if let Some(port) = expected.iter().find(|p| p.name == port_ref.port) {
        return Some(port);
    }
    diagnostics.push(if opposite.iter().any(|p| p.name == port_ref.port) {
        match direction {
            PortDirection::Output => Diagnostic::NotAnOutput {
                conn: conn.clone(),
                port: port_ref.clone(),
            },
            PortDirection::Input => Diagnostic::NotAnInput {
                conn: conn.clone(),
                port: port_ref.clone(),
            },
        }
    } else {
        Diagnostic::UnknownPort {
            conn: conn.clone(),
            port: port_ref.clone(),
        }
    });
    None
}

/// Node id uniqueness + component resolution. Every node's component is
/// resolved and diagnosed, duplicates included; only the first node with a
/// given id claims the contract entry. A hashed node reference that matches
/// several contracts narrows to the one with exactly that hash — hashless
/// table entries match any hash (see [`ComponentRef::matches`]) but an exact
/// hash is the stronger claim. Returns the known node ids and the contract
/// index per uniquely resolvable node.
fn check_nodes<'g>(
    graph: &'g Graph,
    diagnostics: &mut Diagnostics,
) -> (HashSet<&'g NodeId>, HashMap<&'g NodeId, usize>) {
    let mut node_ids: HashSet<&NodeId> = HashSet::new();
    let mut contracts: HashMap<&NodeId, usize> = HashMap::new();
    for node in &graph.nodes {
        let fresh = node_ids.insert(&node.id);
        if !fresh {
            diagnostics.push(Diagnostic::DuplicateNodeId(node.id.clone()));
        }
        let mut matches: Vec<usize> = graph
            .components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.id.matches(&node.component))
            .map(|(i, _)| i)
            .collect();
        if matches.len() > 1 && node.component.content_hash.is_some() {
            let exact: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|&i| graph.components[i].id.content_hash == node.component.content_hash)
                .collect();
            if exact.len() == 1 {
                matches = exact;
            }
        }
        match matches.as_slice() {
            [] => diagnostics.push(Diagnostic::UnknownComponent {
                node: node.id.clone(),
                component: node.component.clone(),
            }),
            [i] => {
                if fresh {
                    contracts.insert(&node.id, *i);
                }
            }
            many => diagnostics.push(Diagnostic::AmbiguousComponent {
                node: node.id.clone(),
                component: node.component.clone(),
                matches: many
                    .iter()
                    .map(|&i| graph.components[i].id.clone())
                    .collect(),
            }),
        }
    }
    (node_ids, contracts)
}

/// The component table is a registry: two entries with the same strict
/// identity (hash included) are redundant at best and, when their ports
/// differ, make every reference to that identity unresolvable.
fn check_duplicate_components(graph: &Graph, diagnostics: &mut Diagnostics) {
    let mut seen: HashSet<&ComponentRef> = HashSet::new();
    for contract in &graph.components {
        if !seen.insert(&contract.id) {
            diagnostics.push(Diagnostic::DuplicateComponent(contract.id.clone()));
        }
    }
}

/// Port names must be unique per contract side — connections address ports by
/// name, so a duplicate makes the port unaddressable.
fn check_duplicate_port_names(graph: &Graph, diagnostics: &mut Diagnostics) {
    for contract in &graph.components {
        for (direction, ports) in [
            (PortDirection::Input, &contract.inputs),
            (PortDirection::Output, &contract.outputs),
        ] {
            let mut seen: HashSet<&PortName> = HashSet::new();
            for port in ports {
                if !seen.insert(&port.name) {
                    diagnostics.push(Diagnostic::DuplicatePortName {
                        component: contract.id.clone(),
                        direction,
                        port: port.name.clone(),
                    });
                }
            }
        }
    }
}

/// Output ports carry neither input-only flag: `optional` and `drained` only
/// make sense on the consuming side.
fn check_output_flags(graph: &Graph, diagnostics: &mut Diagnostics) {
    for contract in &graph.components {
        for output in &contract.outputs {
            if output.optional {
                diagnostics.push(Diagnostic::OptionalOutput {
                    component: contract.id.clone(),
                    port: output.name.clone(),
                });
            }
            if output.drained {
                diagnostics.push(Diagnostic::DrainedOutput {
                    component: contract.id.clone(),
                    port: output.name.clone(),
                });
            }
        }
    }
}

/// Only kinds with completion semantics can be drained: Stream
/// (end-of-stream) and Future (resolution).
fn check_drained_kinds(graph: &Graph, diagnostics: &mut Diagnostics) {
    for contract in &graph.components {
        for input in contract.inputs.iter().filter(|input| input.drained) {
            if matches!(input.kind, PortKind::Value | PortKind::Event) {
                diagnostics.push(Diagnostic::UndrainableInput {
                    component: contract.id.clone(),
                    port: input.name.clone(),
                    kind: input.kind,
                });
            }
        }
    }
}

/// Connection id uniqueness + duplicate endpoints. A feedback and a
/// non-feedback edge between the same ports differ semantically (unit delay
/// vs direct), so only same-flag pairs count as duplicates; the fan-in is
/// still caught by the single-writer check.
fn check_connection_ids(graph: &Graph, diagnostics: &mut Diagnostics) {
    let mut conn_ids: HashSet<&ConnectionId> = HashSet::new();
    let mut endpoints: HashMap<(&PortRef, &PortRef, bool), &ConnectionId> = HashMap::new();
    for conn in &graph.connections {
        if !conn_ids.insert(&conn.id) {
            diagnostics.push(Diagnostic::DuplicateConnectionId(conn.id.clone()));
        }
        match endpoints.entry((&conn.from, &conn.to, conn.feedback)) {
            Entry::Occupied(first) => diagnostics.push(Diagnostic::DuplicateConnection {
                first: (*first.get()).clone(),
                second: conn.id.clone(),
            }),
            Entry::Vacant(slot) => {
                slot.insert(&conn.id);
            }
        }
    }
}

/// Endpoint resolution, direction, kind, type; single-writer fan-in.
///
/// A kind mismatch suppresses the type check on that connection: with the
/// wrong delivery semantics, comparing payload types is noise. Returns the
/// ids of fully-resolved connections (both endpoints found their ports) —
/// the only ones cycle analysis may trust.
fn check_connection_endpoints<'g>(
    graph: &'g Graph,
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, usize>,
    diagnostics: &mut Diagnostics,
) -> HashSet<&'g ConnectionId> {
    let mut resolved: HashSet<&ConnectionId> = HashSet::new();
    let mut writers: BTreeMap<&PortRef, Vec<&ConnectionId>> = BTreeMap::new();
    for conn in &graph.connections {
        let from = resolve_endpoint(
            graph,
            node_ids,
            contracts,
            &conn.id,
            &conn.from,
            PortDirection::Output,
            diagnostics,
        );
        let to = resolve_endpoint(
            graph,
            node_ids,
            contracts,
            &conn.id,
            &conn.to,
            PortDirection::Input,
            diagnostics,
        );
        if to.is_some() {
            writers.entry(&conn.to).or_default().push(&conn.id);
        }
        if let (Some(from), Some(to)) = (from, to) {
            resolved.insert(&conn.id);
            if !from.kind.compatible(to.kind) {
                diagnostics.push(Diagnostic::KindMismatch {
                    conn: conn.id.clone(),
                    from: from.kind,
                    to: to.kind,
                });
            } else if from.ty != to.ty {
                diagnostics.push(Diagnostic::TypeMismatch {
                    conn: conn.id.clone(),
                    from: from.ty.to_string(),
                    to: to.ty.to_string(),
                });
            }
        }
    }
    for (port, connections) in &writers {
        if connections.len() > 1 {
            diagnostics.push(Diagnostic::MultipleWriters {
                port: (*port).clone(),
                connections: connections.iter().map(|c| (*c).clone()).collect(),
            });
        }
    }
    resolved
}

/// Required inputs connected. An incoming connection from a known node
/// counts, even a mistyped one — that defect is already diagnosed
/// separately. A connection whose writer node does not exist can never
/// deliver, so it does not satisfy the input.
fn check_required_inputs(
    graph: &Graph,
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, usize>,
    diagnostics: &mut Diagnostics,
) {
    let connected: HashSet<&PortRef> = graph
        .connections
        .iter()
        .filter(|c| node_ids.contains(&c.from.node))
        .map(|c| &c.to)
        .collect();
    let mut checked: HashSet<&NodeId> = HashSet::new();
    for node in &graph.nodes {
        if !checked.insert(&node.id) {
            continue;
        }
        let Some(&i) = contracts.get(&node.id) else {
            continue;
        };
        for input in &graph.components[i].inputs {
            if input.optional {
                continue;
            }
            let port = PortRef {
                node: node.id.clone(),
                port: input.name.clone(),
            };
            if !connected.contains(&port) {
                diagnostics.push(Diagnostic::RequiredInputUnconnected { port });
            }
        }
    }
}

/// Cycle legality: every cycle needs at least one feedback edge, and every
/// resolved feedback edge should sit on some cycle (unresolved edges never
/// enter cycle analysis, so they cannot fairly be called useless). Returns
/// the topological order, or `None` iff `IllegalCycle` errors were pushed.
fn check_cycles(
    graph: &Graph,
    resolved: &HashSet<&ConnectionId>,
    diagnostics: &mut Diagnostics,
) -> Option<Vec<NodeId>> {
    let order = match topo::acyclic_order(graph, resolved) {
        Ok(order) => Some(order),
        Err(cycles) => {
            for nodes in cycles {
                diagnostics.push(Diagnostic::IllegalCycle { nodes });
            }
            None
        }
    };
    let useful = topo::useful_feedback_edges(graph, resolved);
    for conn in graph.connections.iter().filter(|c| c.feedback) {
        if resolved.contains(&conn.id) && !useful.contains(&conn.id) {
            diagnostics.push(Diagnostic::UselessFeedback {
                conn: conn.id.clone(),
            });
        }
    }
    order
}

/// A feedback edge delivers the previous iteration's value, but a drained
/// input completes before its node's first activation — before any iteration
/// has run — so feedback into a drained input can never deliver, on or off a
/// cycle.
fn check_feedback_into_drained_inputs(
    graph: &Graph,
    contracts: &HashMap<&NodeId, usize>,
    diagnostics: &mut Diagnostics,
) {
    for conn in graph.connections.iter().filter(|c| c.feedback) {
        let Some(&i) = contracts.get(&conn.to.node) else {
            continue;
        };
        let drained = graph.components[i]
            .inputs
            .iter()
            .any(|input| input.drained && input.name == conn.to.port);
        if drained {
            diagnostics.push(Diagnostic::FeedbackIntoDrainedInput {
                conn: conn.id.clone(),
                port: conn.to.clone(),
            });
        }
    }
}

/// A non-feedback connection into a drained input on a cycle waits on a
/// completion that transitively depends on the target node's own output:
/// deadlock. Per-edge precision — a node on a cycle via a reactive port with
/// an off-cycle drained input is fine. Feedback connections are skipped:
/// [`check_feedback_into_drained_inputs`] rejects them unconditionally.
fn check_drained_inputs_off_cycles(
    graph: &Graph,
    contracts: &HashMap<&NodeId, usize>,
    resolved: &HashSet<&ConnectionId>,
    diagnostics: &mut Diagnostics,
) {
    let scc = topo::scc_membership(graph, resolved);
    for conn in graph.connections.iter().filter(|c| !c.feedback) {
        let Some(&i) = contracts.get(&conn.to.node) else {
            continue;
        };
        let drained = graph.components[i]
            .inputs
            .iter()
            .any(|input| input.drained && input.name == conn.to.port);
        if drained
            && let (Some(from), Some(to)) = (scc.get(&conn.from.node), scc.get(&conn.to.node))
            && from == to
        {
            diagnostics.push(Diagnostic::DrainedInputOnCycle {
                conn: conn.id.clone(),
                port: conn.to.clone(),
            });
        }
    }
}

impl Graph {
    /// Consuming typestate transition. On failure the graph is handed back
    /// inside [`CompilationFailure`] together with every diagnostic found.
    pub fn compile(self) -> Result<CompiledGraph, CompilationFailure> {
        let mut diagnostics = Diagnostics::default();

        check_duplicate_components(&self, &mut diagnostics);
        check_duplicate_port_names(&self, &mut diagnostics);
        let (node_ids, contracts) = check_nodes(&self, &mut diagnostics);
        check_output_flags(&self, &mut diagnostics);
        check_drained_kinds(&self, &mut diagnostics);
        check_connection_ids(&self, &mut diagnostics);
        let resolved = check_connection_endpoints(&self, &node_ids, &contracts, &mut diagnostics);
        check_required_inputs(&self, &node_ids, &contracts, &mut diagnostics);
        check_feedback_into_drained_inputs(&self, &contracts, &mut diagnostics);
        // Cycle analysis keys graph vertices by node id, so it needs ids to
        // be unique; duplicates were already diagnosed as errors above.
        let order = if node_ids.len() == self.nodes.len() {
            let order = check_cycles(&self, &resolved, &mut diagnostics);
            check_drained_inputs_off_cycles(&self, &contracts, &resolved, &mut diagnostics);
            order
        } else {
            None
        };

        // `order` is None only when errors were pushed (duplicate node ids
        // or IllegalCycle), so the arms are exhaustive without any panic
        // path.
        match order {
            Some(order) if !diagnostics.has_errors() => Ok(CompiledGraph {
                node_contract: contracts
                    .into_iter()
                    .map(|(id, i)| (id.clone(), i))
                    .collect(),
                graph: self,
                order,
                warnings: diagnostics,
            }),
            _ => Err(CompilationFailure {
                graph: Box::new(self),
                diagnostics,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ComponentRef;
    use crate::port::PortKind;
    use crate::types::Type;

    fn cref(world: &str) -> ComponentRef {
        format!("demo:graph/{world}@0.1.0").parse().unwrap()
    }

    fn contract(
        world: &str,
        inputs: Vec<PortDef>,
        outputs: Vec<PortDef>,
        capabilities: &[&str],
    ) -> ComponentContract {
        ComponentContract {
            id: cref(world),
            inputs,
            outputs,
            capabilities: capabilities.iter().map(|c| Capability::new(*c)).collect(),
            type_names: vec![],
            docs: None,
        }
    }

    fn port(name: &str, kind: PortKind) -> PortDef {
        PortDef::new(name, kind, Type::F64)
    }

    fn source() -> ComponentContract {
        contract(
            "source",
            vec![],
            vec![port("out", PortKind::Value)],
            &["demo:caps/clock", "demo:caps/log"],
        )
    }

    fn sink() -> ComponentContract {
        contract(
            "sink",
            vec![port("in", PortKind::Value)],
            vec![],
            &["demo:caps/log"],
        )
    }

    fn pass_through() -> ComponentContract {
        contract(
            "pass",
            vec![port("in", PortKind::Value)],
            vec![port("out", PortKind::Value)],
            &[],
        )
    }

    fn diags(result: Result<CompiledGraph, CompilationFailure>) -> Vec<Diagnostic> {
        result
            .expect_err("expected compilation failure")
            .diagnostics
            .into_vec()
    }

    #[test]
    fn valid_graph_promotes_and_demotes() {
        let graph = Graph::builder("valid")
            .add_component(source())
            .add_component(sink())
            .add_node("src", cref("source"))
            .add_node("dst", cref("sink"))
            .connect("c1", PortRef::new("src", "out"), PortRef::new("dst", "in"))
            .build();
        let compiled = graph.compile().expect("valid graph");

        let order: Vec<&str> = compiled
            .topological_order()
            .iter()
            .map(|n| n.as_str())
            .collect();
        assert_eq!(order, ["src", "dst"]);
        assert_eq!(
            compiled.required_capabilities(),
            BTreeSet::from([
                Capability::new("demo:caps/clock"),
                Capability::new("demo:caps/log"),
            ]),
            "capabilities deduped across nodes"
        );
        assert!(compiled.warnings().is_empty());
        assert_eq!(compiled.metadata.name, "valid", "Deref reads through");
        assert_eq!(
            compiled.contract_for(&NodeId::from("src")).unwrap().id,
            cref("source")
        );

        let demoted = compiled.into_graph();
        assert_eq!(demoted.nodes.len(), 2);
        demoted.compile().expect("round-trip stays valid");
    }

    #[test]
    fn unknown_component() {
        let graph = Graph::builder("t").add_node("n", cref("ghost")).build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::UnknownComponent {
                node: "n".into(),
                component: cref("ghost"),
            }]
        );
    }

    #[test]
    fn duplicate_node_id() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_node("a", cref("source"))
            .add_node("a", cref("source"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DuplicateNodeId("a".into())]
        );
    }

    #[test]
    fn duplicate_connection_id() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d1", cref("sink"))
            .add_node("d2", cref("sink"))
            .connect("c", PortRef::new("s", "out"), PortRef::new("d1", "in"))
            .connect("c", PortRef::new("s", "out"), PortRef::new("d2", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DuplicateConnectionId("c".into())]
        );
    }

    #[test]
    fn duplicate_connection_endpoints() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .connect("c2", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            diags.contains(&Diagnostic::DuplicateConnection {
                first: "c1".into(),
                second: "c2".into(),
            }),
            "{diags:?}"
        );
        assert!(
            diags.contains(&Diagnostic::MultipleWriters {
                port: PortRef::new("d", "in"),
                connections: vec!["c1".into(), "c2".into()],
            }),
            "{diags:?}"
        );
        assert_eq!(diags.len(), 2);
    }

    #[test]
    fn unknown_node_suppresses_dependent_checks() {
        let graph = Graph::builder("t")
            .add_component(sink())
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("ghost", "out"), PortRef::new("d", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![
                Diagnostic::UnknownNode {
                    conn: "c1".into(),
                    node: "ghost".into(),
                },
                Diagnostic::RequiredInputUnconnected {
                    port: PortRef::new("d", "in"),
                },
            ],
            "no cascading port/kind/type errors from the broken side, but a \
             writer that does not exist cannot satisfy the required input"
        );
    }

    #[test]
    fn unknown_port() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("s", "nope"), PortRef::new("d", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::UnknownPort {
                conn: "c1".into(),
                port: PortRef::new("s", "nope"),
            }]
        );
    }

    #[test]
    fn not_an_output() {
        let graph = Graph::builder("t")
            .add_component(sink())
            .add_node("d1", cref("sink"))
            .add_node("d2", cref("sink"))
            .connect("c1", PortRef::new("d1", "in"), PortRef::new("d2", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            diags.contains(&Diagnostic::NotAnOutput {
                conn: "c1".into(),
                port: PortRef::new("d1", "in"),
            }),
            "{diags:?}"
        );
    }

    #[test]
    fn not_an_input() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_node("s1", cref("source"))
            .add_node("s2", cref("source"))
            .connect("c1", PortRef::new("s1", "out"), PortRef::new("s2", "out"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::NotAnInput {
                conn: "c1".into(),
                port: PortRef::new("s2", "out"),
            }]
        );
    }

    #[test]
    fn kind_mismatch_event_vs_stream() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "emitter",
                vec![],
                vec![port("sig", PortKind::Event)],
                &[],
            ))
            .add_component(contract(
                "consumer",
                vec![port("sig", PortKind::Stream)],
                vec![],
                &[],
            ))
            .add_node("e", cref("emitter"))
            .add_node("c", cref("consumer"))
            .connect("c1", PortRef::new("e", "sig"), PortRef::new("c", "sig"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::KindMismatch {
                conn: "c1".into(),
                from: PortKind::Event,
                to: PortKind::Stream,
            }],
            "same payload type is not enough — kinds must match"
        );
    }

    #[test]
    fn type_mismatch() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(contract(
                "int-sink",
                vec![PortDef::new("in", PortKind::Value, Type::U32)],
                vec![],
                &[],
            ))
            .add_node("s", cref("source"))
            .add_node("d", cref("int-sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::TypeMismatch {
                conn: "c1".into(),
                from: "f64".into(),
                to: "u32".into(),
            }]
        );
    }

    #[test]
    fn multiple_writers() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s1", cref("source"))
            .add_node("s2", cref("source"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("s1", "out"), PortRef::new("d", "in"))
            .connect("c2", PortRef::new("s2", "out"), PortRef::new("d", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::MultipleWriters {
                port: PortRef::new("d", "in"),
                connections: vec!["c1".into(), "c2".into()],
            }]
        );
    }

    #[test]
    fn required_input_unconnected() {
        let graph = Graph::builder("t")
            .add_component(sink())
            .add_node("d", cref("sink"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::RequiredInputUnconnected {
                port: PortRef::new("d", "in"),
            }]
        );
    }

    #[test]
    fn optional_input_may_stay_unconnected() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "opt-sink",
                vec![port("in", PortKind::Value).optional()],
                vec![],
                &[],
            ))
            .add_node("d", cref("opt-sink"))
            .build();
        graph.compile().expect("optional input needs no writer");
    }

    #[test]
    fn mixed_inputs_are_legal() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "mixed",
                vec![port("v", PortKind::Value), port("s", PortKind::Stream)],
                vec![],
                &[],
            ))
            .build();
        graph
            .compile()
            .expect("mixed sync/async inputs color the node async, not invalid");
    }

    #[test]
    fn undrainable_input_rejected() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "drain",
                vec![
                    port("t", PortKind::Event).drained(),
                    port("v", PortKind::Value).drained(),
                ],
                vec![],
                &[],
            ))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![
                Diagnostic::UndrainableInput {
                    component: cref("drain"),
                    port: "t".into(),
                    kind: PortKind::Event,
                },
                Diagnostic::UndrainableInput {
                    component: cref("drain"),
                    port: "v".into(),
                    kind: PortKind::Value,
                },
            ],
            "events never complete and values have no completion — neither drains"
        );
    }

    #[test]
    fn feedback_into_drained_input_rejected() {
        let drain = contract(
            "drain",
            vec![port("in", PortKind::Stream).drained()],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(drain)
            .add_node("a", cref("drain"))
            .connect_feedback("c1", PortRef::new("a", "out"), PortRef::new("a", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::FeedbackIntoDrainedInput {
                conn: "c1".into(),
                port: PortRef::new("a", "in"),
            }],
            "a drain completes before any iteration, so feedback never reaches it"
        );
    }

    #[test]
    fn feedback_into_drained_input_rejected_even_off_cycle() {
        let stream_src = contract(
            "stream-src",
            vec![],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let drain = contract(
            "drain",
            vec![port("in", PortKind::Stream).drained()],
            vec![],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(stream_src)
            .add_component(drain)
            .add_node("s", cref("stream-src"))
            .add_node("d", cref("drain"))
            .connect_feedback("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            diags.contains(&Diagnostic::FeedbackIntoDrainedInput {
                conn: "c1".into(),
                port: PortRef::new("d", "in"),
            }),
            "{diags:?}"
        );
    }

    #[test]
    fn non_feedback_into_drained_input_on_cycle_rejected() {
        let pump = contract(
            "pump",
            vec![port("in", PortKind::Stream)],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let drainer = contract(
            "drainer",
            vec![port("batch", PortKind::Stream).drained()],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(pump)
            .add_component(drainer)
            .add_node("p", cref("pump"))
            .add_node("d", cref("drainer"))
            .connect("c1", PortRef::new("p", "out"), PortRef::new("d", "batch"))
            .connect_feedback("c2", PortRef::new("d", "out"), PortRef::new("p", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DrainedInputOnCycle {
                conn: "c1".into(),
                port: PortRef::new("d", "batch"),
            }],
            "the drain waits on a completion that depends on its own output"
        );
    }

    #[test]
    fn drained_input_off_cycle_validates() {
        let stream_src = contract(
            "stream-src",
            vec![],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let drain = contract(
            "drain",
            vec![port("in", PortKind::Stream).drained()],
            vec![],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(stream_src)
            .add_component(drain)
            .add_node("s", cref("stream-src"))
            .add_node("d", cref("drain"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        graph
            .compile()
            .expect("a drained input fed from off-cycle is legal");
    }

    #[test]
    fn drained_input_beside_on_cycle_reactive_port_validates() {
        let stream_src = contract(
            "stream-src",
            vec![],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let mixed = contract(
            "mixed-drain",
            vec![
                port("batch", PortKind::Stream).drained(),
                port("live", PortKind::Stream),
            ],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(stream_src)
            .add_component(mixed)
            .add_node("s", cref("stream-src"))
            .add_node("m", cref("mixed-drain"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("m", "batch"))
            .connect_feedback("c2", PortRef::new("m", "out"), PortRef::new("m", "live"))
            .build();
        graph.compile().expect(
            "only the drained connection must stay off-cycle; a reactive port may close the loop",
        );
    }

    #[test]
    fn illegal_cycle_without_feedback() {
        let graph = Graph::builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .connect("c1", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect("c2", PortRef::new("b", "out"), PortRef::new("a", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::IllegalCycle {
                nodes: vec!["a".into(), "b".into()],
            }]
        );
    }

    #[test]
    fn feedback_edge_legalizes_cycle() {
        let graph = Graph::builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .connect("c1", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect_feedback("c2", PortRef::new("b", "out"), PortRef::new("a", "in"))
            .build();
        let compiled = graph.compile().expect("feedback cycle is legal");
        assert!(compiled.warnings().is_empty());
        let order: Vec<&str> = compiled
            .topological_order()
            .iter()
            .map(|n| n.as_str())
            .collect();
        assert_eq!(order, ["a", "b"], "feedback edge imposes no ordering");
    }

    #[test]
    fn useless_feedback_is_a_warning() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect_feedback("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let compiled = graph.compile().expect("warnings do not reject");
        let warnings: Vec<_> = compiled.warnings().iter().collect();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            &Diagnostic::UselessFeedback { conn: "c1".into() }
        );
    }

    #[test]
    fn collects_all_diagnostics() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("a", cref("source"))
            .add_node("a", cref("source"))
            .add_node("g", cref("ghost"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("x", "out"), PortRef::new("d", "in"))
            .build();
        let diags = diags(graph.compile());
        assert_eq!(diags.len(), 4, "{diags:?}");
        assert!(diags.contains(&Diagnostic::DuplicateNodeId("a".into())));
        assert!(diags.contains(&Diagnostic::RequiredInputUnconnected {
            port: PortRef::new("d", "in"),
        }));
        assert!(diags.contains(&Diagnostic::UnknownComponent {
            node: "g".into(),
            component: cref("ghost"),
        }));
        assert!(diags.contains(&Diagnostic::UnknownNode {
            conn: "c1".into(),
            node: "x".into(),
        }));
    }

    #[test]
    fn failure_hands_the_graph_back() {
        let graph = Graph::builder("hand-back")
            .add_node("n", cref("ghost"))
            .build();
        let mut failure = graph.compile().unwrap_err();
        assert_eq!(failure.graph.metadata.name, "hand-back");
        assert!(failure.to_string().contains("1 error"));

        failure.graph.components.push(source());
        failure.graph.nodes[0].component = cref("source");
        (*failure.graph).compile().expect("corrected graph retries");
    }

    #[test]
    fn empty_graph_compiles() {
        let compiled = Graph::builder("empty").build().compile().expect("empty");
        assert!(compiled.topological_order().is_empty());
        assert!(compiled.warnings().is_empty());
    }

    #[test]
    fn duplicate_component() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(source())
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DuplicateComponent(cref("source"))]
        );
    }

    #[test]
    fn duplicate_port_name() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "twice",
                vec![port("in", PortKind::Value), port("in", PortKind::Stream)],
                vec![],
                &[],
            ))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DuplicatePortName {
                component: cref("twice"),
                direction: PortDirection::Input,
                port: "in".into(),
            }]
        );
    }

    #[test]
    fn kind_mismatch_suppresses_type_check() {
        let graph = Graph::builder("t")
            .add_component(contract(
                "emitter",
                vec![],
                vec![PortDef::new("sig", PortKind::Event, Type::F64)],
                &[],
            ))
            .add_component(contract(
                "consumer",
                vec![PortDef::new("sig", PortKind::Stream, Type::U32)],
                vec![],
                &[],
            ))
            .add_node("e", cref("emitter"))
            .add_node("c", cref("consumer"))
            .connect("c1", PortRef::new("e", "sig"), PortRef::new("c", "sig"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::KindMismatch {
                conn: "c1".into(),
                from: PortKind::Event,
                to: PortKind::Stream,
            }],
            "with the wrong kind, a payload-type diagnostic would be noise"
        );
    }

    #[test]
    fn failure_carries_warnings_alongside_errors() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .add_node("d2", cref("sink"))
            .connect_feedback("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            diags.contains(&Diagnostic::RequiredInputUnconnected {
                port: PortRef::new("d2", "in"),
            }),
            "{diags:?}"
        );
        assert!(
            diags.contains(&Diagnostic::UselessFeedback { conn: "c1".into() }),
            "{diags:?}"
        );
    }

    #[test]
    fn ambiguous_component() {
        let mut a = source();
        a.id.content_hash = Some("aa".into());
        let mut b = source();
        b.id.content_hash = Some("bb".into());
        let graph = Graph::builder("t")
            .add_component(a.clone())
            .add_component(b.clone())
            .add_node("n", cref("source"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::AmbiguousComponent {
                node: "n".into(),
                component: cref("source"),
                matches: vec![a.id, b.id],
            }]
        );
    }

    #[test]
    fn hashed_reference_disambiguates() {
        let mut a = source();
        a.id.content_hash = Some("aa".into());
        let mut b = source();
        b.id.content_hash = Some("bb".into());
        let mut node_ref = cref("source");
        node_ref.content_hash = Some("bb".into());
        let graph = Graph::builder("t")
            .add_component(a)
            .add_component(b)
            .add_node("n", node_ref)
            .build();
        graph
            .compile()
            .expect("hashed reference selects exactly one contract");
    }

    #[test]
    fn hashed_reference_narrows_past_unhashed_entries() {
        let mut hashed = source();
        hashed.id.content_hash = Some("bb".into());
        let mut node_ref = cref("source");
        node_ref.content_hash = Some("bb".into());
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(hashed)
            .add_node("n", node_ref)
            .build();
        let compiled = graph
            .compile()
            .expect("an exact hash outranks a hashless match");
        assert_eq!(
            compiled
                .contract_for(&NodeId::from("n"))
                .unwrap()
                .id
                .content_hash,
            Some("bb".into())
        );
    }

    #[test]
    fn output_flags_rejected() {
        let flagged = contract(
            "flagged",
            vec![],
            vec![
                port("a", PortKind::Value).optional(),
                port("b", PortKind::Stream).drained(),
            ],
            &[],
        );
        let graph = Graph::builder("t")
            .add_component(flagged)
            .add_node("n", cref("flagged"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![
                Diagnostic::OptionalOutput {
                    component: cref("flagged"),
                    port: "a".into(),
                },
                Diagnostic::DrainedOutput {
                    component: cref("flagged"),
                    port: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn duplicate_node_ids_suppress_cycle_analysis() {
        let graph = Graph::builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("a", cref("pass"))
            .connect("c1", PortRef::new("a", "out"), PortRef::new("a", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::DuplicateNodeId("a".into())],
            "cycle analysis is suppressed when node ids collide"
        );
    }

    #[test]
    fn feedback_and_forward_edges_are_distinct_connections() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .connect_feedback("c2", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::DuplicateConnection { .. })),
            "{diags:?}"
        );
        assert!(diags.contains(&Diagnostic::MultipleWriters {
            port: PortRef::new("d", "in"),
            connections: vec!["c1".into(), "c2".into()],
        }));
    }
}
