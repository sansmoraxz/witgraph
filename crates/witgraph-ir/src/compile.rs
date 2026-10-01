//! Graph compilation: the `Graph → CompiledGraph` typestate transition.
//!
//! Compilation resolves the graph's component table against a
//! [`ContractSource`] (contracts are never stored in the graph) and
//! validates the graph, collecting ALL diagnostics (never fail-fast, never
//! panics).
//! Checks whose preconditions failed on a connection (unresolvable endpoint)
//! are suppressed on that connection rather than cascading noise.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
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

/// Supplies the contract for a component reference at compile time.
///
/// Contracts are re-derived from WIT rather than stored in the graph. A
/// lookup matches on the full [`ComponentRef`], content hash included, so a
/// graph pinned to one revision of a component never compiles against
/// another. An unhashed reference falls back to the single contract with the
/// same package, world and version, if exactly one exists.
pub trait ContractSource {
    /// The contract for `id`, if this source has one.
    fn contract(&self, id: &ComponentRef) -> Option<&ComponentContract>;
}

fn lookup<'a, I>(contracts: I, id: &ComponentRef) -> Option<&'a ComponentContract>
where
    I: Iterator<Item = &'a ComponentContract> + Clone,
{
    if let Some(exact) = contracts.clone().find(|c| c.id == *id) {
        return Some(exact);
    }
    if id.content_hash.is_some() {
        return None;
    }
    let mut matching = contracts.filter(|c| c.id.matches(id));
    match (matching.next(), matching.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

impl ContractSource for [ComponentContract] {
    fn contract(&self, id: &ComponentRef) -> Option<&ComponentContract> {
        lookup(self.iter(), id)
    }
}

impl ContractSource for Vec<ComponentContract> {
    fn contract(&self, id: &ComponentRef) -> Option<&ComponentContract> {
        self.as_slice().contract(id)
    }
}

impl<S: BuildHasher> ContractSource for HashMap<ComponentRef, ComponentContract, S> {
    fn contract(&self, id: &ComponentRef) -> Option<&ComponentContract> {
        self.get(id).or_else(|| lookup(self.values(), id))
    }
}

/// A graph that compiled successfully.
///
/// Sealed — no public constructor, private fields, not deserializable. The
/// only way in is [`Graph::compile`]; the way back out (for mutation) is
/// [`CompiledGraph::into_graph`].
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    graph: Graph,
    contracts: Vec<ComponentContract>,
    node_contract: HashMap<NodeId, usize>,
    order: Vec<NodeId>,
    depth: HashMap<NodeId, usize>,
    islands: Vec<Vec<NodeId>>,
    island_of: HashMap<NodeId, usize>,
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

    /// Longest-path depth from source nodes over non-feedback edges.
    ///
    /// Nodes with no predecessors have depth 0; each other node has
    /// `max(depth of predecessors) + 1`. Nodes at the same depth have no
    /// path between them, so a runtime may re-run them together.
    pub fn depth_map(&self) -> &HashMap<NodeId, usize> {
        &self.depth
    }

    /// The graph's stream islands: the connected components of the graph
    /// over Stream and Future connections. Nodes in one island exchange
    /// component-model stream/future handles directly, so a runtime must
    /// host them together; a node with no Stream or Future connection is an
    /// island of its own.
    ///
    /// Deterministic: islands are ordered by the topological position of
    /// their first member, and members are in topological order.
    pub fn islands(&self) -> &[Vec<NodeId>] {
        &self.islands
    }

    /// The index into [`islands`](Self::islands) of the island holding the
    /// given node.
    pub fn island_of(&self, node: &NodeId) -> Option<usize> {
        self.island_of.get(node).copied()
    }

    /// The contract the given node instantiates.
    pub fn contract_for(&self, node: &NodeId) -> Option<&ComponentContract> {
        self.node_contract.get(node).map(|&i| &self.contracts[i])
    }

    /// The resolved contract of every component table entry, in table
    /// order.
    pub fn contracts(&self) -> &[ComponentContract] {
        &self.contracts
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

fn resolve_endpoint<'c>(
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, &'c ComponentContract>,
    conn: &ConnectionId,
    port_ref: &PortRef,
    direction: PortDirection,
    diagnostics: &mut Diagnostics,
) -> Option<&'c PortDef> {
    if !node_ids.contains(&port_ref.node) {
        diagnostics.push(Diagnostic::UnknownNode {
            conn: conn.clone(),
            node: port_ref.node.clone(),
        });
        return None;
    }
    // A known node with an unknown component or a missing contract was
    // already diagnosed; suppress dependent checks.
    let contract = *contracts.get(&port_ref.node)?;
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
/// of each uniquely resolvable node whose table entry has one.
fn check_nodes<'g, 'c>(
    graph: &'g Graph,
    table: &'c [Option<ComponentContract>],
    diagnostics: &mut Diagnostics,
) -> (
    HashSet<&'g NodeId>,
    HashMap<&'g NodeId, &'c ComponentContract>,
) {
    let mut node_ids: HashSet<&NodeId> = HashSet::new();
    let mut contracts: HashMap<&NodeId, &ComponentContract> = HashMap::new();
    for node in &graph.nodes {
        let fresh = node_ids.insert(&node.id);
        if !fresh {
            diagnostics.push(Diagnostic::DuplicateNodeId(node.id.clone()));
        }
        let mut matches: Vec<usize> = graph
            .components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.matches(&node.component))
            .map(|(i, _)| i)
            .collect();
        if matches.len() > 1 && node.component.content_hash.is_some() {
            let exact: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|&i| graph.components[i].content_hash == node.component.content_hash)
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
                if fresh && let Some(contract) = &table[*i] {
                    contracts.insert(&node.id, contract);
                }
            }
            many => diagnostics.push(Diagnostic::AmbiguousComponent {
                node: node.id.clone(),
                component: node.component.clone(),
                matches: many.iter().map(|&i| graph.components[i].clone()).collect(),
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
    for component in &graph.components {
        if !seen.insert(component) {
            diagnostics.push(Diagnostic::DuplicateComponent(component.clone()));
        }
    }
}

/// Port names must be unique per contract side — connections address ports by
/// name, so a duplicate makes the port unaddressable.
fn check_duplicate_port_names(table: &[Option<ComponentContract>], diagnostics: &mut Diagnostics) {
    for contract in table.iter().flatten() {
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

/// `optional` is a Value-input flag: an output is never optional, and an
/// unconnected stream or future input has no handle to hand the node.
fn check_port_flags(table: &[Option<ComponentContract>], diagnostics: &mut Diagnostics) {
    for contract in table.iter().flatten() {
        for output in contract.outputs.iter().filter(|output| output.optional) {
            diagnostics.push(Diagnostic::OptionalOutput {
                component: contract.id.clone(),
                port: output.name.clone(),
            });
        }
        for input in contract
            .inputs
            .iter()
            .filter(|input| input.optional && input.kind.is_async())
        {
            diagnostics.push(Diagnostic::OptionalAsyncInput {
                component: contract.id.clone(),
                port: input.name.clone(),
                kind: input.kind,
            });
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
    contracts: &HashMap<&NodeId, &ComponentContract>,
    diagnostics: &mut Diagnostics,
) -> HashSet<&'g ConnectionId> {
    let mut resolved: HashSet<&ConnectionId> = HashSet::new();
    let mut writers: BTreeMap<&PortRef, Vec<&ConnectionId>> = BTreeMap::new();
    for conn in &graph.connections {
        let from = resolve_endpoint(
            node_ids,
            contracts,
            &conn.id,
            &conn.from,
            PortDirection::Output,
            diagnostics,
        );
        let to = resolve_endpoint(
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
                    from: from.type_display(),
                    to: to.type_display(),
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
    contracts: &HashMap<&NodeId, &ComponentContract>,
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
        let Some(contract) = contracts.get(&node.id) else {
            continue;
        };
        for input in &contract.inputs {
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

/// The kind of the output port a connection reads from, if it resolves.
fn source_kind(
    contracts: &HashMap<&NodeId, &ComponentContract>,
    from: &PortRef,
) -> Option<PortKind> {
    contracts
        .get(&from.node)?
        .outputs
        .iter()
        .find(|output| output.name == from.port)
        .map(|output| output.kind)
}

/// Feedback edges are unit-delay boundaries between generations; only a
/// Value can cross one. A stream or future handle belongs to the run that
/// created it.
fn check_feedback_kinds(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    diagnostics: &mut Diagnostics,
) {
    for conn in graph.connections.iter().filter(|c| c.feedback) {
        if let Some(kind) = source_kind(contracts, &conn.from)
            && kind.is_async()
        {
            diagnostics.push(Diagnostic::AsyncFeedback {
                conn: conn.id.clone(),
                port: conn.from.clone(),
                kind,
            });
        }
    }
}

/// A stream or future handle moves to exactly one consumer, so an async
/// output may feed at most one connection.
fn check_async_fan_out(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    diagnostics: &mut Diagnostics,
) {
    let mut readers: BTreeMap<&PortRef, (PortKind, Vec<ConnectionId>)> = BTreeMap::new();
    for conn in &graph.connections {
        if let Some(kind) = source_kind(contracts, &conn.from)
            && kind.is_async()
        {
            readers
                .entry(&conn.from)
                .or_insert_with(|| (kind, Vec::new()))
                .1
                .push(conn.id.clone());
        }
    }
    for (port, (kind, connections)) in readers {
        if connections.len() > 1 {
            diagnostics.push(Diagnostic::AsyncFanOut {
                port: port.clone(),
                kind,
                connections,
            });
        }
    }
}

/// Union-find over resolved Stream/Future connections, read off in
/// topological order (see [`CompiledGraph::islands`]).
fn stream_islands(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    resolved: &HashSet<&ConnectionId>,
    order: &[NodeId],
) -> (Vec<Vec<NodeId>>, HashMap<NodeId, usize>) {
    let index: HashMap<&NodeId, usize> = order.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let mut parent: Vec<usize> = (0..order.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for conn in &graph.connections {
        if !resolved.contains(&conn.id)
            || !source_kind(contracts, &conn.from).is_some_and(PortKind::is_async)
        {
            continue;
        }
        if let (Some(&a), Some(&b)) = (index.get(&conn.from.node), index.get(&conn.to.node)) {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            // The earlier topological position becomes the root, so a root
            // is always its island's first member.
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut islands: Vec<Vec<NodeId>> = Vec::new();
    let mut island_of: HashMap<NodeId, usize> = HashMap::new();
    let mut root_island: HashMap<usize, usize> = HashMap::new();
    for (i, node) in order.iter().enumerate() {
        let root = find(&mut parent, i);
        let island = *root_island.entry(root).or_insert_with(|| {
            islands.push(Vec::new());
            islands.len() - 1
        });
        islands[island].push(node.clone());
        island_of.insert(node.clone(), island);
    }
    (islands, island_of)
}

impl Graph {
    /// Consuming typestate transition. Every component table entry is
    /// resolved through `contracts`; an entry with no contract is diagnosed
    /// ([`Diagnostic::ContractNotFound`]). On failure the graph is handed
    /// back inside [`CompilationFailure`] together with every diagnostic
    /// found.
    pub fn compile(
        self,
        contracts: &(impl ContractSource + ?Sized),
    ) -> Result<CompiledGraph, CompilationFailure> {
        let mut diagnostics = Diagnostics::default();

        let table: Vec<Option<ComponentContract>> = self
            .components
            .iter()
            .map(|id| {
                let contract = contracts.contract(id).cloned();
                if contract.is_none() {
                    diagnostics.push(Diagnostic::ContractNotFound(id.clone()));
                }
                contract
            })
            .collect();

        check_duplicate_components(&self, &mut diagnostics);
        check_duplicate_port_names(&table, &mut diagnostics);
        let (node_ids, node_contracts) = check_nodes(&self, &table, &mut diagnostics);
        check_port_flags(&table, &mut diagnostics);
        check_connection_ids(&self, &mut diagnostics);
        let resolved =
            check_connection_endpoints(&self, &node_ids, &node_contracts, &mut diagnostics);
        check_required_inputs(&self, &node_ids, &node_contracts, &mut diagnostics);
        check_feedback_kinds(&self, &node_contracts, &mut diagnostics);
        check_async_fan_out(&self, &node_contracts, &mut diagnostics);
        // Cycle analysis keys graph vertices by node id, so it needs ids to
        // be unique; duplicates were already diagnosed as errors above.
        let order = if node_ids.len() == self.nodes.len() {
            check_cycles(&self, &resolved, &mut diagnostics)
        } else {
            None
        };

        // `order` is None only when errors were pushed (duplicate node ids
        // or IllegalCycle), so the arms are exhaustive without any panic
        // path.
        match order {
            Some(order) if !diagnostics.has_errors() => {
                let depth = topo::depth_map(&self, &order, &resolved);
                let (islands, island_of) =
                    stream_islands(&self, &node_contracts, &resolved, &order);
                // Compact the table to its resolved entries (all of them, on
                // success) and point each node at its contract.
                let mut compact: HashMap<&ComponentRef, usize> = HashMap::new();
                let mut resolved_contracts: Vec<ComponentContract> = Vec::new();
                for contract in table.iter().flatten() {
                    compact.entry(&contract.id).or_insert_with(|| {
                        resolved_contracts.push(contract.clone());
                        resolved_contracts.len() - 1
                    });
                }
                let node_contract: HashMap<NodeId, usize> = node_contracts
                    .iter()
                    .filter_map(|(node, contract)| {
                        compact.get(&contract.id).map(|&i| ((*node).clone(), i))
                    })
                    .collect();
                Ok(CompiledGraph {
                    contracts: resolved_contracts,
                    node_contract,
                    graph: self,
                    order,
                    depth,
                    islands,
                    island_of,
                    warnings: diagnostics,
                })
            }
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
    use crate::Type;
    use crate::graph::{GraphBuilder, ResourceClaim};
    use crate::id::ComponentRef;
    use crate::port::PortKind;

    /// A [`GraphBuilder`] that also keeps the contracts it registers, so the
    /// built graph can be compiled against them.
    struct TestBuilder {
        builder: GraphBuilder,
        contracts: Vec<ComponentContract>,
    }

    struct TestGraph {
        graph: Graph,
        contracts: Vec<ComponentContract>,
    }

    fn builder(name: &str) -> TestBuilder {
        TestBuilder {
            builder: Graph::builder(name),
            contracts: Vec::new(),
        }
    }

    impl TestBuilder {
        fn add_component(mut self, contract: ComponentContract) -> Self {
            self.builder = self.builder.add_component(&contract);
            self.contracts.push(contract);
            self
        }

        fn add_node(mut self, id: &str, component: ComponentRef) -> Self {
            self.builder = self.builder.add_node(id, component);
            self
        }

        fn connect(mut self, id: &str, from: PortRef, to: PortRef) -> Self {
            self.builder = self.builder.connect(id, from, to);
            self
        }

        fn connect_feedback(mut self, id: &str, from: PortRef, to: PortRef) -> Self {
            self.builder = self.builder.connect_feedback(id, from, to);
            self
        }

        fn set_resource(
            mut self,
            node: &str,
            resource: &str,
            claim: ResourceClaim,
        ) -> Result<Self, String> {
            self.builder = self.builder.set_resource(node, resource, claim)?;
            Ok(self)
        }

        fn build(self) -> TestGraph {
            TestGraph {
                graph: self.builder.build(),
                contracts: self.contracts,
            }
        }
    }

    impl TestGraph {
        fn compile(self) -> Result<CompiledGraph, CompilationFailure> {
            self.graph.compile(&self.contracts)
        }
    }

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
            run: crate::component::RunKind::Sync,
            capabilities: capabilities.iter().map(|c| Capability::new(*c)).collect(),
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
        let graph = builder("valid")
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

        let contracts = compiled.contracts().to_vec();
        let demoted = compiled.into_graph();
        assert_eq!(demoted.nodes.len(), 2);
        demoted.compile(&contracts).expect("round-trip stays valid");
    }

    #[test]
    fn missing_contract_is_diagnosed() {
        let graph = Graph::builder("t")
            .add_component(source())
            .add_node("s", cref("source"))
            .build();
        assert_eq!(
            diags(graph.compile(&[] as &[ComponentContract])),
            vec![Diagnostic::ContractNotFound(cref("source"))],
            "a table entry with no contract suppresses the node's dependent checks"
        );
    }

    #[test]
    fn content_hash_mismatch_is_rejected() {
        let mut pinned = cref("source");
        pinned.content_hash = Some("bb".into());
        let mut rebuilt = source();
        rebuilt.id.content_hash = Some("aa".into());
        let graph = Graph::builder("t")
            .add_component(pinned.clone())
            .add_node("s", cref("source"))
            .build();
        assert_eq!(
            diags(graph.compile([rebuilt].as_slice())),
            vec![Diagnostic::ContractNotFound(pinned)],
            "a graph pinned to one revision never compiles against another"
        );
    }

    #[test]
    fn unhashed_entry_resolves_to_the_unique_hashed_contract() {
        let mut hashed = source();
        hashed.id.content_hash = Some("aa".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("s", cref("source"))
            .build();
        let compiled = graph.compile([hashed].as_slice()).expect("unique match");
        assert_eq!(
            compiled.contracts()[0].id.content_hash.as_deref(),
            Some("aa")
        );
    }

    #[test]
    fn hash_map_is_a_contract_source() {
        let contracts: HashMap<ComponentRef, ComponentContract> = [source(), sink()]
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_component(cref("sink"))
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        let compiled = graph.compile(&contracts).expect("valid graph");
        assert_eq!(
            compiled.contract_for(&NodeId::from("d")).unwrap().id,
            cref("sink")
        );
    }

    #[test]
    fn unknown_component() {
        let graph = builder("t").add_node("n", cref("ghost")).build();
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
    fn kind_mismatch_value_vs_stream() {
        let graph = builder("t")
            .add_component(contract(
                "emitter",
                vec![],
                vec![port("sig", PortKind::Value)],
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
                from: PortKind::Value,
                to: PortKind::Stream,
            }],
            "same payload type is not enough — kinds must match"
        );
    }

    #[test]
    fn type_mismatch() {
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
            .add_component(contract(
                "mixed",
                vec![port("v", PortKind::Value), port("s", PortKind::Stream)],
                vec![],
                &[],
            ))
            .build();
        graph
            .compile()
            .expect("value and stream inputs may mix on one node");
    }

    fn stream_source() -> ComponentContract {
        contract(
            "stream-src",
            vec![],
            vec![port("out", PortKind::Stream)],
            &[],
        )
    }

    fn stream_sink() -> ComponentContract {
        contract(
            "stream-sink",
            vec![port("in", PortKind::Stream)],
            vec![],
            &[],
        )
    }

    #[test]
    fn stream_one_to_one_validates() {
        let graph = builder("t")
            .add_component(stream_source())
            .add_component(stream_sink())
            .add_node("s", cref("stream-src"))
            .add_node("d", cref("stream-sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .build();
        graph.compile().expect("a stream output may feed one input");
    }

    #[test]
    fn async_fan_out_rejected() {
        let graph = builder("t")
            .add_component(stream_source())
            .add_component(stream_sink())
            .add_node("s", cref("stream-src"))
            .add_node("d1", cref("stream-sink"))
            .add_node("d2", cref("stream-sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d1", "in"))
            .connect("c2", PortRef::new("s", "out"), PortRef::new("d2", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::AsyncFanOut {
                port: PortRef::new("s", "out"),
                kind: PortKind::Stream,
                connections: vec!["c1".into(), "c2".into()],
            }],
            "a stream handle moves to exactly one consumer"
        );
    }

    #[test]
    fn value_fan_out_is_legal() {
        let graph = builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d1", cref("sink"))
            .add_node("d2", cref("sink"))
            .connect("c1", PortRef::new("s", "out"), PortRef::new("d1", "in"))
            .connect("c2", PortRef::new("s", "out"), PortRef::new("d2", "in"))
            .build();
        graph
            .compile()
            .expect("values are copied, so they fan out freely");
    }

    #[test]
    fn optional_async_input_rejected() {
        let graph = builder("t")
            .add_component(contract(
                "opt-stream",
                vec![port("s", PortKind::Stream).optional()],
                vec![],
                &[],
            ))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::OptionalAsyncInput {
                component: cref("opt-stream"),
                port: "s".into(),
                kind: PortKind::Stream,
            }]
        );
    }

    #[test]
    fn feedback_on_stream_rejected() {
        let pump = contract(
            "pump",
            vec![port("in", PortKind::Stream)],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let graph = builder("t")
            .add_component(pump)
            .add_node("p", cref("pump"))
            .connect_feedback("c1", PortRef::new("p", "out"), PortRef::new("p", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::AsyncFeedback {
                conn: "c1".into(),
                port: PortRef::new("p", "out"),
                kind: PortKind::Stream,
            }],
            "a stream handle cannot cross a generation boundary"
        );
    }

    #[test]
    fn stream_cycle_through_value_feedback_validates() {
        let producer = contract(
            "producer",
            vec![port("seed", PortKind::Value).optional()],
            vec![port("items", PortKind::Stream)],
            &[],
        );
        let reducer = contract(
            "reducer",
            vec![port("items", PortKind::Stream)],
            vec![port("total", PortKind::Value)],
            &[],
        );
        let graph = builder("t")
            .add_component(producer)
            .add_component(reducer)
            .add_node("p", cref("producer"))
            .add_node("r", cref("reducer"))
            .connect("c1", PortRef::new("p", "items"), PortRef::new("r", "items"))
            .connect_feedback("c2", PortRef::new("r", "total"), PortRef::new("p", "seed"))
            .build();
        let compiled = graph
            .compile()
            .expect("a value feedback edge breaks the loop into generations");
        assert_eq!(compiled.islands().len(), 1, "p and r share a stream island");
    }

    #[test]
    fn islands_partition_over_async_edges() {
        let value_to_stream = contract(
            "v2s",
            vec![port("in", PortKind::Value)],
            vec![port("out", PortKind::Stream)],
            &[],
        );
        let stream_to_value = contract(
            "s2v",
            vec![port("in", PortKind::Stream)],
            vec![port("out", PortKind::Value)],
            &[],
        );
        let future_src = contract(
            "future-src",
            vec![],
            vec![port("done", PortKind::Future)],
            &[],
        );
        let future_sink = contract(
            "future-sink",
            vec![port("done", PortKind::Future)],
            vec![],
            &[],
        );
        let graph = builder("t")
            .add_component(source())
            .add_component(sink())
            .add_component(value_to_stream)
            .add_component(stream_to_value)
            .add_component(future_src)
            .add_component(future_sink)
            .add_node("src", cref("source"))
            .add_node("a", cref("v2s"))
            .add_node("b", cref("s2v"))
            .add_node("dst", cref("sink"))
            .add_node("f", cref("future-src"))
            .add_node("g", cref("future-sink"))
            .connect("c1", PortRef::new("src", "out"), PortRef::new("a", "in"))
            .connect("c2", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect("c3", PortRef::new("b", "out"), PortRef::new("dst", "in"))
            .connect("c4", PortRef::new("f", "done"), PortRef::new("g", "done"))
            .build();
        let compiled = graph.compile().expect("valid graph");
        let mut islands: Vec<Vec<&str>> = compiled
            .islands()
            .iter()
            .map(|island| island.iter().map(|n| n.as_str()).collect())
            .collect();
        islands.sort();
        assert_eq!(
            islands,
            vec![vec!["a", "b"], vec!["dst"], vec!["f", "g"], vec!["src"]],
            "value edges never join islands; stream and future edges do"
        );
        for island in compiled.islands() {
            let positions: Vec<usize> = island
                .iter()
                .map(|n| {
                    compiled
                        .topological_order()
                        .iter()
                        .position(|m| m == n)
                        .unwrap()
                })
                .collect();
            assert!(
                positions.windows(2).all(|w| w[0] < w[1]),
                "members in topo order"
            );
        }
        let a = compiled.island_of(&NodeId::from("a")).unwrap();
        assert_eq!(compiled.island_of(&NodeId::from("b")), Some(a));
        assert_ne!(compiled.island_of(&NodeId::from("src")), Some(a));
        assert_eq!(compiled.island_of(&NodeId::from("ghost")), None);
    }

    #[test]
    fn illegal_cycle_without_feedback() {
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("hand-back").add_node("n", cref("ghost")).build();
        let mut failure = graph.compile().unwrap_err();
        assert_eq!(failure.graph.metadata.name, "hand-back");
        assert!(failure.to_string().contains("1 error"));

        failure.graph.components.push(cref("source"));
        failure.graph.nodes[0].component = cref("source");
        (*failure.graph)
            .compile([source()].as_slice())
            .expect("corrected graph retries");
    }

    #[test]
    fn empty_graph_compiles() {
        let compiled = builder("empty").build().compile().expect("empty");
        assert!(compiled.topological_order().is_empty());
        assert!(compiled.warnings().is_empty());
    }

    #[test]
    fn duplicate_component() {
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
            .add_component(contract(
                "emitter",
                vec![],
                vec![PortDef::new("sig", PortKind::Value, Type::F64)],
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
                from: PortKind::Value,
                to: PortKind::Stream,
            }],
            "with the wrong kind, a payload-type diagnostic would be noise"
        );
    }

    #[test]
    fn failure_carries_warnings_alongside_errors() {
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
        let graph = builder("t")
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
            vec![port("a", PortKind::Value).optional()],
            &[],
        );
        let graph = builder("t")
            .add_component(flagged)
            .add_node("n", cref("flagged"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::OptionalOutput {
                component: cref("flagged"),
                port: "a".into(),
            }]
        );
    }

    #[test]
    fn duplicate_node_ids_suppress_cycle_analysis() {
        let graph = builder("t")
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
        let graph = builder("t")
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

    #[test]
    fn feedback_on_future_rejected() {
        let future_node = contract(
            "future-node",
            vec![port("trigger", PortKind::Value)],
            vec![port("result", PortKind::Future)],
            &[],
        );
        let graph = builder("t")
            .add_component(future_node)
            .add_node("a", cref("future-node"))
            .connect_feedback(
                "c1",
                PortRef::new("a", "result"),
                PortRef::new("a", "trigger"),
            )
            .build();
        let diags_list = diags(graph.compile());
        assert!(
            diags_list.contains(&Diagnostic::AsyncFeedback {
                conn: "c1".into(),
                port: PortRef::new("a", "result"),
                kind: PortKind::Future,
            }),
            "feedback on future port should be rejected: {diags_list:?}"
        );
    }

    #[test]
    fn resource_budget_valid() {
        use crate::graph::Fraction;
        assert!(Fraction::new(0.5).is_ok());
        assert!(Fraction::new(1.0).is_ok());
        assert!(Fraction::new(0.001).is_ok());
    }

    #[test]
    fn resource_budget_invalid_rejected_by_fraction() {
        use crate::graph::Fraction;
        assert!(Fraction::new(0.0).is_err());
        assert!(Fraction::new(-0.1).is_err());
        assert!(Fraction::new(1.5).is_err());
        assert!(Fraction::new(f64::NAN).is_err());
        assert!(Fraction::new(f64::INFINITY).is_err());
        assert!(Fraction::new(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn resource_budget_compiles() {
        let graph = builder("t")
            .add_component(source())
            .add_node("s", cref("source"))
            .set_resource("s", "disk", ResourceClaim::new(0.5).unwrap())
            .unwrap()
            .build();
        graph.compile().expect("valid resource budget");
    }

    #[test]
    fn resource_budget_unknown_node_rejected() {
        let result = builder("t")
            .add_component(source())
            .add_node("s", cref("source"))
            .set_resource("ghost", "disk", ResourceClaim::new(0.5).unwrap());
        assert!(result.is_err(), "set_resource on unknown node should fail");
    }
}
