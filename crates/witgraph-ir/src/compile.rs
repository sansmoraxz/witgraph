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

use crate::component::ComponentContract;
use crate::diagnostics::{Diagnostic, Diagnostics};
use crate::graph::{Connection, Graph, Link, Node};
use crate::id::{ComponentRef, ConnectionId, NodeId, PackageRef, PortName, PortRef, ResourceId};
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

/// Supplies the contracts for component references at compile time.
///
/// Contracts are re-derived from WIT rather than stored in the graph. A
/// lookup matches on the full [`ComponentRef`], content hash included, so a
/// graph pinned to one revision of a component never compiles against
/// another. An unhashed reference resolves to the single contract with the
/// same package, world and version, if exactly one exists: the
/// *single-match rule*, [`resolve_contract`].
///
/// A source only lists [`candidates`](Self::candidates); compilation applies
/// the rule itself, so it tells a missing component
/// ([`Diagnostic::ContractNotFound`]) from an ambiguous one
/// ([`Diagnostic::AmbiguousContract`]). It does not trust an implementation
/// either: a candidate whose package, world, version or (for a pinned
/// entry) content hash differs from the entry is ignored.
///
/// The slice, array and `Vec` sources scan their contracts on every lookup;
/// for a large source, use a [`ContractIndex`].
pub trait ContractSource {
    /// Every contract this source holds that could resolve `id`: the same
    /// package (version included) and world, and for a pinned `id` exactly
    /// its content hash. The order should be deterministic; it is the
    /// order an [`AmbiguousContract`](Diagnostic::AmbiguousContract) lists
    /// them in.
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract>;

    /// The contract `id` resolves to under the single-match rule, if any.
    fn contract(&self, id: &ComponentRef) -> Option<&ComponentContract> {
        match resolve_contract(id, self.candidates(id)) {
            Resolution::Found(contract) => Some(contract),
            Resolution::NotFound | Resolution::Ambiguous(_) => None,
        }
    }
}

/// Whether a contract with id `found` may resolve the table entry
/// `requested`: the same package (version included) and world, and, when
/// the entry is pinned, exactly its content hash.
fn fits(requested: &ComponentRef, found: &ComponentRef) -> bool {
    requested.package == found.package
        && requested.world == found.world
        && (requested.content_hash.is_none() || requested.content_hash == found.content_hash)
}

/// How a component reference resolves among candidate contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// Exactly one contract fits.
    Found(&'a ComponentContract),
    /// None does.
    NotFound,
    /// Several contracts fit an unhashed reference: their ids, sorted.
    Ambiguous(Vec<ComponentRef>),
}

/// The single-match rule: the contract among `candidates` that `id`
/// resolves to. Candidates that do not fit `id` are ignored, and copies
/// count once: contracts with one content hash (which covers everything
/// but doc comments), or hashless ones with one id that are equal but for
/// their doc comments. Two hashless contracts with one id that differ
/// otherwise are ambiguous. A pinned `id` therefore finds at most one.
pub fn resolve_contract<'a>(
    id: &ComponentRef,
    candidates: impl IntoIterator<Item = &'a ComponentContract>,
) -> Resolution<'a> {
    let mut found: Vec<&ComponentContract> = Vec::new();
    for candidate in candidates {
        let copy = found.iter().any(|f| {
            f.id == candidate.id && (f.id.content_hash.is_some() || same_but_docs(f, candidate))
        });
        if fits(id, &candidate.id) && !copy {
            found.push(candidate);
        }
    }
    match found.as_slice() {
        [] => Resolution::NotFound,
        [only] => Resolution::Found(only),
        many => {
            let mut ids: Vec<ComponentRef> = many.iter().map(|c| c.id.clone()).collect();
            ids.sort();
            Resolution::Ambiguous(ids)
        }
    }
}

/// Whether two contracts are equal but for their doc comments: what a
/// content hash covers.
fn same_but_docs(a: &ComponentContract, b: &ComponentContract) -> bool {
    let ports = |x: &[PortDef], y: &[PortDef]| {
        x.len() == y.len()
            && x.iter().zip(y).all(|(p, q)| {
                p.name == q.name && p.kind == q.kind && p.ty == q.ty && p.optional == q.optional
            })
    };
    a.id == b.id
        && a.capabilities == b.capabilities
        && ports(&a.inputs, &b.inputs)
        && ports(&a.outputs, &b.outputs)
}

impl ContractSource for [ComponentContract] {
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract> {
        self.iter().filter(|c| fits(id, &c.id)).collect()
    }
}

impl<const N: usize> ContractSource for [ComponentContract; N] {
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract> {
        self.as_slice().candidates(id)
    }
}

impl ContractSource for Vec<ComponentContract> {
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract> {
        self.as_slice().candidates(id)
    }
}

impl<S: BuildHasher> ContractSource for HashMap<ComponentRef, ComponentContract, S> {
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract> {
        // A pinned key is looked up directly; anything else (an unpinned
        // key, or a contract stored under another key) needs a scan, sorted
        // so the outcome does not depend on the map's iteration order.
        if id.content_hash.is_some()
            && let Some(contract) = self.get(id).filter(|c| fits(id, &c.id))
        {
            return vec![contract];
        }
        let mut found: Vec<&ComponentContract> =
            self.values().filter(|c| fits(id, &c.id)).collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found
    }
}

/// Contracts indexed by package (version included) and world: a
/// [`ContractSource`] whose lookups cost the number of revisions of that
/// one world, not the size of the source.
#[derive(Debug, Clone, Default)]
pub struct ContractIndex {
    by_world: HashMap<(PackageRef, String), Vec<ComponentContract>>,
}

impl ContractIndex {
    /// Indexes `contracts`.
    pub fn new(contracts: impl IntoIterator<Item = ComponentContract>) -> Self {
        contracts.into_iter().collect()
    }

    /// Adds a contract.
    pub fn insert(&mut self, contract: ComponentContract) {
        self.by_world
            .entry((contract.id.package.clone(), contract.id.world.clone()))
            .or_default()
            .push(contract);
    }
}

impl FromIterator<ComponentContract> for ContractIndex {
    fn from_iter<I: IntoIterator<Item = ComponentContract>>(iter: I) -> Self {
        let mut index = Self::default();
        for contract in iter {
            index.insert(contract);
        }
        index
    }
}

impl ContractSource for ContractIndex {
    fn candidates(&self, id: &ComponentRef) -> Vec<&ComponentContract> {
        self.by_world
            .get(&(id.package.clone(), id.world.clone()))
            .into_iter()
            .flatten()
            .filter(|c| fits(id, &c.id))
            .collect()
    }
}

/// What the host must provide under one link name, for one interface,
/// across every component a graph instantiates. See
/// [`CompiledGraph::required_capabilities`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequiredCapability {
    /// The link name: a full interface id, an inline interface's import
    /// name, a `func:`- or `resource:`-prefixed root name, or a label (see
    /// [`Capability`](crate::Capability)).
    pub interface: String,
    /// For a label, the full id of the interface it stands for; `None`
    /// otherwise. Components that use one label for different interfaces
    /// get an entry each.
    pub implements: Option<String>,
    /// Every item some component imports under this name, mapped to each
    /// signature it is imported with. One signature is the norm; several
    /// mean components disagree, and the host must link those nodes
    /// differently (capabilities are linked per node).
    pub items: BTreeMap<String, BTreeSet<String>>,
}

impl RequiredCapability {
    /// Whether components import some item under this name with
    /// different signatures.
    pub fn conflicts(&self) -> bool {
        self.items.values().any(|signatures| signatures.len() > 1)
    }
}

/// A graph that compiled successfully.
///
/// Sealed — no public constructor, private fields, not deserializable. The
/// only way in is [`Graph::compile`]; the way back out (for mutation) is
/// [`CompiledGraph::into_graph`].
///
/// Besides the graph, it keeps what compilation resolved: every node's
/// contract and island ([`nodes`](Self::nodes)) and every connection's
/// ports ([`connections`](Self::connections)), so consumers need not look
/// them up again.
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    graph: Graph,
    contracts: Vec<ComponentContract>,
    /// Each node's position in `graph.nodes`.
    node_index: HashMap<NodeId, usize>,
    /// Per node (by position): its contract's index in `contracts`.
    node_contract: Vec<usize>,
    /// Per node (by position): its island.
    node_island: Vec<usize>,
    /// Per connection (by position): where its ends resolved.
    connection_ends: Vec<Ends>,
    /// Per node (by position): the positions of its links in
    /// `graph.links`, in declaration order.
    node_links: Vec<Vec<usize>>,
    order: Vec<NodeId>,
    islands: Vec<Vec<NodeId>>,
    warnings: Diagnostics,
}

/// Where a connection's ends resolved: node positions, and port indices
/// among the source's outputs and the target's inputs.
#[derive(Debug, Clone, Copy)]
struct Ends {
    from_node: usize,
    output: usize,
    to_node: usize,
    input: usize,
}

/// A node of a [`CompiledGraph`], with what compilation resolved for it.
#[derive(Debug, Clone, Copy)]
pub struct CompiledNode<'a> {
    /// The node's position in [`Graph::nodes`]: a dense index.
    pub index: usize,
    /// The node.
    pub node: &'a Node,
    /// The contract it instantiates.
    pub contract: &'a ComponentContract,
    /// Its island's index into [`CompiledGraph::islands`].
    pub island: usize,
}

/// A connection of a [`CompiledGraph`], with its resolved ports.
#[derive(Debug, Clone, Copy)]
pub struct CompiledConnection<'a> {
    /// The connection.
    pub connection: &'a Connection,
    /// The position of its source node in [`Graph::nodes`].
    pub from_node: usize,
    /// The output port it reads.
    pub from: &'a PortDef,
    /// The position of its target node in [`Graph::nodes`].
    pub to_node: usize,
    /// The input port it writes.
    pub to: &'a PortDef,
    /// Whether it delivers an `option<T>` output to an optional input of
    /// payload `T`, unwrapped ([`PortDef::unwraps_into`]).
    pub unwraps_option: bool,
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

    /// Execution order over non-feedback edges. Among the nodes ready at
    /// each step, the earliest declared comes first, so the order is
    /// stable: the same graph always orders the same way.
    pub fn topological_order(&self) -> &[NodeId] {
        &self.order
    }

    /// Longest-path depth from source nodes over non-feedback edges,
    /// computed on each call.
    ///
    /// Nodes with no predecessors have depth 0; each other node has
    /// `max(depth of predecessors) + 1`. Nodes at the same depth have no
    /// path between them, so a runtime may re-run them together.
    pub fn depth_map(&self) -> HashMap<NodeId, usize> {
        let resolved = vec![true; self.graph.connections.len()];
        topo::depth_map(&self.graph, &self.order, &resolved)
    }

    /// The graph's stream islands: the connected components of the graph
    /// over Stream and Future connections. Nodes in one island exchange
    /// component-model stream/future handles directly, so a runtime must
    /// host them together; a node with no Stream or Future connection is an
    /// island of its own.
    ///
    /// The islands form a DAG over non-feedback connections: islands that a
    /// non-feedback path leaves and re-enters (through Value connections)
    /// are merged into one, with a [`Diagnostic::MergedIsland`] warning.
    ///
    /// Deterministic: islands are in a topological order of the island DAG
    /// (an island comes after every island that reaches it), ties broken by
    /// their first member's topological position, and members are in
    /// topological order.
    pub fn islands(&self) -> &[Vec<NodeId>] {
        &self.islands
    }

    /// The index into [`islands`](Self::islands) of the island holding the
    /// given node, if the graph has it.
    pub fn island_of(&self, node: &NodeId) -> Option<usize> {
        self.node(node).map(|n| n.island)
    }

    /// The contract the given node instantiates, if the graph has it.
    pub fn contract_for(&self, node: &NodeId) -> Option<&ComponentContract> {
        self.node(node).map(|n| n.contract)
    }

    /// The given node, with its contract and island, if the graph has it.
    pub fn node(&self, id: &NodeId) -> Option<CompiledNode<'_>> {
        self.node_at(*self.node_index.get(id)?)
    }

    fn node_at(&self, index: usize) -> Option<CompiledNode<'_>> {
        Some(CompiledNode {
            index,
            node: self.graph.nodes.get(index)?,
            contract: self.contracts.get(*self.node_contract.get(index)?)?,
            island: *self.node_island.get(index)?,
        })
    }

    /// Every node, in declaration order, with its contract and island.
    pub fn nodes(&self) -> impl Iterator<Item = CompiledNode<'_>> + '_ {
        // Every index is in range: compilation resolved every node.
        (0..self.graph.nodes.len()).filter_map(|i| self.node_at(i))
    }

    /// Every connection, in declaration order, with its resolved ports.
    pub fn connections(&self) -> impl Iterator<Item = CompiledConnection<'_>> + '_ {
        // Every port index is in range: compilation resolved every
        // connection.
        self.graph
            .connections
            .iter()
            .zip(&self.connection_ends)
            .filter_map(|(connection, ends)| {
                let from = self
                    .node_at(ends.from_node)?
                    .contract
                    .outputs
                    .get(ends.output)?;
                let to = self
                    .node_at(ends.to_node)?
                    .contract
                    .inputs
                    .get(ends.input)?;
                Some(CompiledConnection {
                    connection,
                    from_node: ends.from_node,
                    from,
                    to_node: ends.to_node,
                    to,
                    unwraps_option: from.unwraps_into(to),
                })
            })
    }

    /// The distinct contracts the component table resolved to, in table
    /// order of first appearance (entries resolving to the same contract
    /// share it).
    pub fn contracts(&self) -> &[ComponentContract] {
        &self.contracts
    }

    /// What the host must provide for every component a node instantiates:
    /// one entry per link name and labelled interface, sorted by them, with
    /// every item imported under that name and each signature it is
    /// imported with. The result
    /// depends only on which contracts the nodes instantiate and which of
    /// their imports are linked, not on the nodes' order.
    ///
    /// An import a [`Link`] satisfies is not required of the host. What the
    /// link's provider imports in turn is, but it is not listed here: the
    /// graph does not hold providers' contracts, so it is known only when
    /// the graph is loaded.
    pub fn required_capabilities(&self) -> Vec<RequiredCapability> {
        // By link name and labelled interface: each item's signatures.
        type Items = BTreeMap<String, BTreeSet<String>>;
        let mut required: BTreeMap<(&str, Option<&str>), Items> = BTreeMap::new();
        // Nodes of one contract with the same imports linked contribute
        // the same: each such pair is walked once.
        let mut seen: HashMap<usize, Vec<BTreeSet<&str>>> = HashMap::new();
        for (node, &index) in self.node_contract.iter().enumerate() {
            let linked: BTreeSet<&str> = self
                .node_links
                .get(node)
                .into_iter()
                .flatten()
                .filter_map(|&position| self.graph.links.get(position))
                .map(|link| link.import.as_str())
                .collect();
            let Some(contract) = self.contracts.get(index) else {
                continue;
            };
            let walked = seen.entry(index).or_default();
            if walked.contains(&linked) {
                continue;
            }
            for capability in &contract.capabilities {
                // A link covers its import, and an import wit-component
                // merges it into (a semver-compatible version of the same
                // interface), as loading does.
                let covered = linked.iter().any(|import| {
                    capability.is_interface()
                        && crate::interface::semver_compatible(import, &capability.interface)
                });
                if covered {
                    continue;
                }
                let key = (
                    capability.interface.as_str(),
                    capability.implements.as_deref(),
                );
                let items = required.entry(key).or_default();
                for (name, signature) in &capability.items {
                    items
                        .entry(name.clone())
                        .or_default()
                        .insert(signature.clone());
                }
            }
            walked.push(linked);
        }
        required
            .into_iter()
            .map(|((interface, implements), items)| RequiredCapability {
                interface: interface.to_string(),
                implements: implements.map(str::to_string),
                items,
            })
            .collect()
    }

    /// The links satisfying imports of `node`, in declaration order.
    pub fn links_of<'a>(&'a self, node: &'a NodeId) -> impl Iterator<Item = &'a Link> {
        self.node_index
            .get(node)
            .and_then(|&index| self.node_links.get(index))
            .into_iter()
            .flatten()
            .filter_map(|&position| self.graph.links.get(position))
    }
}

/// A port a connection endpoint resolved to: its index among its side of
/// the contract, and the port.
type Endpoint<'c> = (usize, &'c PortDef);

fn resolve_endpoint<'c>(
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, &'c ComponentContract>,
    conn: &ConnectionId,
    port_ref: &PortRef,
    direction: PortDirection,
    diagnostics: &mut Diagnostics,
) -> Option<Endpoint<'c>> {
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
    if let Some(found) = expected
        .iter()
        .enumerate()
        .find(|(_, p)| p.name == port_ref.port)
    {
        return Some(found);
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

/// How one node component reference resolves against the table.
enum Reference<'c> {
    /// No table entry matches it.
    Unknown,
    /// One does: its contract, if the entry resolved.
    Bound(Option<&'c ComponentContract>),
    /// Several distinct ones do: their ids, sorted.
    Ambiguous(Vec<ComponentRef>),
}

/// Node id uniqueness + component resolution. Every node's component is
/// resolved and diagnosed, duplicates included; only the first node with a
/// given id claims the contract entry.
///
/// A node reference is matched against what each table entry *resolved
/// to* (the contract's id, content hash included), falling back to the
/// entry itself when it has no contract. A hashed reference needs exactly
/// its hash: it never binds to a contract with another hash, nor to a
/// hashless one, even through an unhashed entry (an unresolved unhashed
/// entry, already diagnosed, still matches, so its nodes add no noise).
/// Entries that resolved to the same contract are one match. Each distinct
/// reference is resolved once, and an ambiguous one is reported once, with
/// every node using it. Returns the known node ids and the contract of each
/// uniquely resolvable node whose table entry has one.
fn check_nodes<'g, 'c>(
    graph: &'g Graph,
    table: &[Option<&'c ComponentContract>],
    diagnostics: &mut Diagnostics,
) -> (
    HashSet<&'g NodeId>,
    HashMap<&'g NodeId, &'c ComponentContract>,
) {
    let effective: Vec<&ComponentRef> = graph
        .components
        .iter()
        .zip(table)
        .map(|(entry, contract)| contract.map_or(entry, |c| &c.id))
        .collect();
    // Entries by package and world, so each reference looks at its own
    // world's entries only.
    let mut by_world: HashMap<(&PackageRef, &str), Vec<usize>> = HashMap::new();
    for (i, id) in effective.iter().enumerate() {
        by_world
            .entry((&id.package, id.world.as_str()))
            .or_default()
            .push(i);
    }
    let resolve = |reference: &ComponentRef| -> Reference<'c> {
        let entries = by_world
            .get(&(&reference.package, reference.world.as_str()))
            .map_or(&[][..], Vec::as_slice);
        let mut matches: Vec<usize> = match &reference.content_hash {
            None => entries.to_vec(),
            Some(hash) => {
                let exact: Vec<usize> = entries
                    .iter()
                    .copied()
                    .filter(|&i| effective[i].content_hash.as_ref() == Some(hash))
                    .collect();
                if exact.is_empty() {
                    entries
                        .iter()
                        .copied()
                        .filter(|&i| table[i].is_none() && effective[i].content_hash.is_none())
                        .collect()
                } else {
                    exact
                }
            }
        };
        let mut ids: Vec<&ComponentRef> = matches.iter().map(|&i| effective[i]).collect();
        ids.sort();
        ids.dedup();
        match ids.as_slice() {
            [] => Reference::Unknown,
            [_] => {
                matches.retain(|&i| table[i].is_some());
                Reference::Bound(matches.first().and_then(|&i| table[i]))
            }
            many => Reference::Ambiguous(many.iter().map(|id| (*id).clone()).collect()),
        }
    };

    let mut node_ids: HashSet<&NodeId> = HashSet::new();
    let mut contracts: HashMap<&NodeId, &ComponentContract> = HashMap::new();
    let mut references: HashMap<&ComponentRef, Reference<'c>> = HashMap::new();
    // Ambiguous references, in order of first use, with their nodes.
    let mut ambiguous: Vec<(&ComponentRef, Vec<NodeId>)> = Vec::new();
    let mut ambiguous_at: HashMap<&ComponentRef, usize> = HashMap::new();
    for node in &graph.nodes {
        let fresh = node_ids.insert(&node.id);
        if !fresh {
            diagnostics.push(Diagnostic::DuplicateNodeId(node.id.clone()));
        }
        let reference = references
            .entry(&node.component)
            .or_insert_with(|| resolve(&node.component));
        match reference {
            Reference::Unknown => diagnostics.push(Diagnostic::UnknownComponent {
                node: node.id.clone(),
                component: node.component.clone(),
            }),
            Reference::Bound(contract) => {
                if fresh && let Some(contract) = contract {
                    contracts.insert(&node.id, contract);
                }
            }
            Reference::Ambiguous(_) => {
                let at = *ambiguous_at.entry(&node.component).or_insert_with(|| {
                    ambiguous.push((&node.component, Vec::new()));
                    ambiguous.len() - 1
                });
                ambiguous[at].1.push(node.id.clone());
            }
        }
    }
    for (component, nodes) in ambiguous {
        if let Some(Reference::Ambiguous(matches)) = references.remove(component) {
            diagnostics.push(Diagnostic::AmbiguousComponent {
                nodes,
                component: component.clone(),
                matches,
            });
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

/// The table's resolved contracts, each once (entries resolving to the same
/// contract are checked, and diagnosed, once).
fn distinct<'c>(
    table: &[Option<&'c ComponentContract>],
) -> impl Iterator<Item = &'c ComponentContract> {
    let mut seen: HashSet<&ComponentRef> = HashSet::new();
    table
        .iter()
        .flatten()
        .copied()
        .filter(move |c| seen.insert(&c.id))
}

/// Port names must be unique per contract side — connections address ports by
/// name, so a duplicate makes the port unaddressable.
fn check_duplicate_port_names(table: &[Option<&ComponentContract>], diagnostics: &mut Diagnostics) {
    for contract in distinct(table) {
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
/// unconnected stream or future input has no handle to hand the node. A
/// Value port always has a payload type; only a bare stream or future has
/// none.
fn check_port_flags(table: &[Option<&ComponentContract>], diagnostics: &mut Diagnostics) {
    for contract in distinct(table) {
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
        for (direction, ports) in [
            (PortDirection::Input, &contract.inputs),
            (PortDirection::Output, &contract.outputs),
        ] {
            for port in ports
                .iter()
                .filter(|p| p.kind == PortKind::Value && p.ty.is_none())
            {
                diagnostics.push(Diagnostic::UntypedValuePort {
                    component: contract.id.clone(),
                    direction,
                    port: port.name.clone(),
                });
            }
        }
    }
}

/// Checks every link: its id is unique, its node exists, and it names an
/// interface import of the node's component that no earlier link already
/// satisfies. Whether the provider's export fits the import is not known
/// here: the graph does not hold the provider's contract, and loading
/// checks it against the components themselves.
fn check_links(
    graph: &Graph,
    node_ids: &HashSet<&NodeId>,
    node_contracts: &HashMap<&NodeId, &ComponentContract>,
    diagnostics: &mut Diagnostics,
) {
    let mut ids = HashSet::new();
    // Per node, its links so far.
    let mut linked: HashMap<&NodeId, Vec<&Link>> = HashMap::new();
    for link in &graph.links {
        if !ids.insert(&link.id) {
            diagnostics.push(Diagnostic::DuplicateLinkId(link.id.clone()));
        }
        if !node_ids.contains(&link.node) {
            diagnostics.push(Diagnostic::LinkUnknownNode {
                link: link.id.clone(),
                node: link.node.clone(),
            });
            continue;
        }
        // A node whose contract did not resolve is already diagnosed. The
        // import is matched as loading matches it: by name, else a
        // semver-compatible version, so the same link compiles against a
        // contract lowered from WIT and one lowered from the bytes (where
        // wit-component merged such versions into one import).
        if let Some(contract) = node_contracts.get(&link.node)
            && !crate::interface::resolve_import(&contract.capabilities, &link.import)
                .is_some_and(|c| c.is_interface())
        {
            diagnostics.push(Diagnostic::UnknownImport {
                link: link.id.clone(),
                node: link.node.clone(),
                import: link.import.clone(),
            });
            continue;
        }
        // The same import, or one a component merges it with (a
        // semver-compatible version of the same interface): either way, one
        // import of the built component.
        let earlier = linked.entry(&link.node).or_default();
        match earlier
            .iter()
            .find(|first| crate::interface::semver_compatible(&first.import, &link.import))
        {
            Some(first) => diagnostics.push(Diagnostic::ImportLinkedTwice {
                node: link.node.clone(),
                import: link.import.clone(),
                first: first.id.clone(),
                second: link.id.clone(),
            }),
            None => earlier.push(link),
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

/// What [`check_connection_endpoints`] resolved.
struct Endpoints<'g> {
    /// Per connection (by position): the resolved source output and target
    /// input indices, if both endpoints found their ports — the only
    /// connections later analysis may trust.
    ports: Vec<Option<(usize, usize)>>,
    /// The connections writing each input port that resolved, from a node
    /// that exists (a writer whose node does not exist can never deliver).
    writers: BTreeMap<&'g PortRef, Vec<&'g Connection>>,
}

impl Endpoints<'_> {
    fn resolved(&self) -> Vec<bool> {
        self.ports.iter().map(Option::is_some).collect()
    }
}

/// Endpoint resolution, direction, kind, type; single-writer fan-in.
///
/// A kind mismatch suppresses the type check on that connection: with the
/// wrong delivery semantics, comparing payload types is noise.
fn check_connection_endpoints<'g>(
    graph: &'g Graph,
    node_ids: &HashSet<&NodeId>,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    diagnostics: &mut Diagnostics,
) -> Endpoints<'g> {
    let mut ports = vec![None; graph.connections.len()];
    let mut writers: BTreeMap<&PortRef, Vec<&Connection>> = BTreeMap::new();
    for (index, conn) in graph.connections.iter().enumerate() {
        let from = resolve_endpoint(
            node_ids,
            contracts,
            &conn.id,
            &conn.from,
            PortDirection::Output,
            diagnostics,
        );
        // Both ends on one unknown node: that node is reported once.
        let to = if conn.to.node == conn.from.node && !node_ids.contains(&conn.to.node) {
            None
        } else {
            resolve_endpoint(
                node_ids,
                contracts,
                &conn.id,
                &conn.to,
                PortDirection::Input,
                diagnostics,
            )
        };
        if to.is_some() && node_ids.contains(&conn.from.node) {
            writers.entry(&conn.to).or_default().push(conn);
        }
        if let (Some((output, from)), Some((input, to))) = (from, to) {
            ports[index] = Some((output, input));
            if !from.kind.compatible(to.kind) {
                diagnostics.push(Diagnostic::KindMismatch {
                    conn: conn.id.clone(),
                    from: from.kind,
                    to: to.kind,
                });
            } else if from.ty != to.ty && !from.unwraps_into(to) {
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
                connections: connections.iter().map(|c| c.id.clone()).collect(),
            });
        }
    }
    Endpoints { ports, writers }
}

/// Required inputs connected. An incoming connection from a known node
/// counts, even a mistyped one — that defect is already diagnosed
/// separately. A connection whose writer node does not exist can never
/// deliver, so it does not satisfy the input. A required input fed only by
/// feedback connections is legal but has no value until one is injected,
/// which gets a warning.
fn check_required_inputs(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    writers: &BTreeMap<&PortRef, Vec<&Connection>>,
    diagnostics: &mut Diagnostics,
) {
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
            match writers.get(&port).map(Vec::as_slice) {
                None | Some([]) => {
                    diagnostics.push(Diagnostic::RequiredInputUnconnected { port });
                }
                // A Stream or Future feedback edge is an error of its own.
                Some(conns)
                    if input.kind == PortKind::Value && conns.iter().all(|c| c.feedback) =>
                {
                    diagnostics.push(Diagnostic::FeedbackOnlyInput {
                        port,
                        conn: conns[0].id.clone(),
                    });
                }
                Some(_) => {}
            }
        }
    }
}

/// Cycle legality: every cycle needs at least one feedback edge. Returns
/// the topological order, or `None` iff `IllegalCycle` errors were pushed.
fn check_cycles(
    graph: &Graph,
    resolved: &[bool],
    diagnostics: &mut Diagnostics,
) -> Option<Vec<NodeId>> {
    match topo::acyclic_order(graph, resolved) {
        Ok(order) => Some(order),
        Err(cycles) => {
            for nodes in cycles {
                diagnostics.push(Diagnostic::IllegalCycle { nodes });
            }
            None
        }
    }
}

/// Every resolved feedback edge should earn its unit delay: it sits on a
/// cycle of nodes, or on a cycle of islands (with every feedback edge on it
/// a plain connection, the islands on it would merge). Unresolved edges
/// never enter cycle analysis, so they cannot fairly be called useless.
fn check_useless_feedback(
    graph: &Graph,
    resolved: &[bool],
    islands: &Islands,
    diagnostics: &mut Diagnostics,
) {
    let useful = topo::useful_feedback_edges(graph, resolved);
    // Islands on a common cycle over every resolved connection, feedback
    // included.
    let island_edges = graph
        .connections
        .iter()
        .zip(resolved)
        .filter(|(_, ok)| **ok)
        .filter_map(|(conn, _)| {
            Some((
                *islands.island_of.get(&conn.from.node)?,
                *islands.island_of.get(&conn.to.node)?,
            ))
        });
    let mut group_of: HashMap<usize, usize> = HashMap::new();
    for (group, members) in topo::cyclic_groups(islands.islands.len(), island_edges)
        .into_iter()
        .enumerate()
    {
        for island in members {
            group_of.insert(island, group);
        }
    }
    for (index, conn) in graph.connections.iter().enumerate() {
        if !conn.feedback || !resolved[index] || useful.contains(&index) {
            continue;
        }
        let separates = match (
            islands.island_of.get(&conn.from.node),
            islands.island_of.get(&conn.to.node),
        ) {
            (Some(from), Some(to)) => {
                from != to
                    && group_of
                        .get(from)
                        .is_some_and(|g| group_of.get(to) == Some(g))
            }
            _ => false,
        };
        if !separates {
            diagnostics.push(Diagnostic::UselessFeedback {
                conn: conn.id.clone(),
            });
        }
    }
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
/// output may feed at most one connection. Only connections whose both
/// endpoints resolved count: one into an unknown node or port takes
/// nothing (its defect is reported on its own).
fn check_async_fan_out(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    resolved: &[bool],
    diagnostics: &mut Diagnostics,
) {
    let mut readers: BTreeMap<&PortRef, (PortKind, Vec<ConnectionId>)> = BTreeMap::new();
    for (conn, _) in graph
        .connections
        .iter()
        .zip(resolved)
        .filter(|(_, ok)| **ok)
    {
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

/// Claims within an island are held together for the whole generation
/// (see [`ResourceClaim`](crate::ResourceClaim)), so an island whose
/// members' claims on one resource sum to more than all of it could never
/// start.
fn check_island_claims(graph: &Graph, islands: &Islands, diagnostics: &mut Diagnostics) {
    // Tolerates float rounding in sums of fractions such as 0.1 + 0.2.
    const EPSILON: f64 = 1e-9;
    let mut nodes: HashMap<&NodeId, &Node> = HashMap::new();
    for node in &graph.nodes {
        nodes.entry(&node.id).or_insert(node);
    }
    for members in &islands.islands {
        let mut claims: BTreeMap<&ResourceId, f64> = BTreeMap::new();
        for node in members.iter().filter_map(|m| nodes.get(m)) {
            for (resource, claim) in &node.resources {
                *claims.entry(resource).or_insert(0.0) += claim.fraction.get();
            }
        }
        for (resource, _) in claims.into_iter().filter(|(_, sum)| *sum > 1.0 + EPSILON) {
            diagnostics.push(Diagnostic::IslandOverclaims {
                nodes: members.clone(),
                resource: resource.clone(),
            });
        }
    }
}

/// The graph's islands (see [`CompiledGraph::islands`]), read off in
/// topological order, plus the islands that had to absorb a cycle.
///
/// Two passes of union-find over the nodes in topological order:
/// 1. resolved Stream/Future connections join their endpoints;
/// 2. the islands from pass 1 that form a cycle over non-feedback
///    connections (a Value path that leaves an island and re-enters it) are
///    joined too, so the islands themselves form a DAG. Otherwise neither
///    island on such a cycle could finish before the other started.
fn stream_islands(
    graph: &Graph,
    contracts: &HashMap<&NodeId, &ComponentContract>,
    resolved: &[bool],
    order: &[NodeId],
) -> Islands {
    let index: HashMap<&NodeId, usize> = order.iter().enumerate().map(|(i, n)| (n, i)).collect();
    // A set's root is its smallest member: the earliest topological
    // position, so a root is always its island's first member.
    let mut parent = crate::partition::Partition::new(order.len());
    let edges: Vec<(usize, usize, bool)> = graph
        .connections
        .iter()
        .zip(resolved)
        .filter(|(conn, ok)| **ok && !conn.feedback)
        .map(|(conn, _)| conn)
        .filter_map(|conn| {
            let is_async = source_kind(contracts, &conn.from).is_some_and(PortKind::is_async);
            Some((
                *index.get(&conn.from.node)?,
                *index.get(&conn.to.node)?,
                is_async,
            ))
        })
        .collect();
    for &(a, b, is_async) in &edges {
        if is_async {
            parent.union(a, b);
        }
    }

    let roots: Vec<usize> = (0..order.len()).map(|i| parent.find(i)).collect();
    let cycles = topo::cyclic_groups(
        order.len(),
        edges
            .iter()
            .filter(|(a, b, _)| roots[*a] != roots[*b])
            .map(|&(a, b, _)| (roots[a], roots[b])),
    );
    let mut merged_roots = Vec::new();
    for group in &cycles {
        if let [first, rest @ ..] = group.as_slice() {
            for &other in rest {
                parent.union(*first, other);
            }
            merged_roots.push(*first);
        }
    }

    // Islands keyed by root (their first member's position), then numbered
    // in a topological order of the island DAG, earliest root first among
    // the ready ones.
    let roots: Vec<usize> = (0..order.len()).map(|i| parent.find(i)).collect();
    let mut successors: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    let mut indegree: BTreeMap<usize, usize> = roots.iter().map(|&r| (r, 0)).collect();
    for &(a, b, _) in &edges {
        let (ra, rb) = (roots[a], roots[b]);
        if ra != rb && successors.entry(ra).or_default().insert(rb) {
            *indegree.entry(rb).or_insert(0) += 1;
        }
    }
    let mut ready: BTreeSet<usize> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(r, _)| *r)
        .collect();
    let mut number: HashMap<usize, usize> = HashMap::new();
    while let Some(root) = ready.pop_first() {
        number.insert(root, number.len());
        for next in successors.get(&root).into_iter().flatten() {
            if let Some(d) = indegree.get_mut(next) {
                *d -= 1;
                if *d == 0 {
                    ready.insert(*next);
                }
            }
        }
    }

    let mut islands: Vec<Vec<NodeId>> = vec![Vec::new(); number.len()];
    let mut island_of: HashMap<NodeId, usize> = HashMap::new();
    for (i, node) in order.iter().enumerate() {
        if let Some(&island) = number.get(&roots[i]) {
            islands[island].push(node.clone());
            island_of.insert(node.clone(), island);
        }
    }
    let merged = merged_roots
        .into_iter()
        .filter_map(|root| number.get(&parent.find(root)).copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|island| islands[island].clone())
        .collect();
    Islands {
        islands,
        island_of,
        merged,
    }
}

/// The result of [`stream_islands`].
struct Islands {
    /// In a topological order of the island DAG.
    islands: Vec<Vec<NodeId>>,
    island_of: HashMap<NodeId, usize>,
    /// Members of every island that absorbed a cycle, in island order.
    merged: Vec<Vec<NodeId>>,
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

        // Each distinct entry is resolved, and diagnosed, once; a repeated
        // entry (already a DuplicateComponent) gets the same outcome.
        let mut resolved_entries: HashMap<&ComponentRef, Option<&ComponentContract>> =
            HashMap::new();
        let table: Vec<Option<&ComponentContract>> = self
            .components
            .iter()
            .map(|id| {
                *resolved_entries.entry(id).or_insert_with(|| {
                    // A source may hand back anything; the rule only counts
                    // candidates that fit the entry.
                    match resolve_contract(id, contracts.candidates(id)) {
                        Resolution::Found(contract) => Some(contract),
                        Resolution::NotFound => {
                            diagnostics.push(Diagnostic::ContractNotFound(id.clone()));
                            None
                        }
                        Resolution::Ambiguous(matches) => {
                            diagnostics.push(Diagnostic::AmbiguousContract {
                                component: id.clone(),
                                matches,
                            });
                            None
                        }
                    }
                })
            })
            .collect();

        check_duplicate_components(&self, &mut diagnostics);
        check_duplicate_port_names(&table, &mut diagnostics);
        let (node_ids, node_contracts) = check_nodes(&self, &table, &mut diagnostics);
        check_port_flags(&table, &mut diagnostics);
        check_connection_ids(&self, &mut diagnostics);
        check_links(&self, &node_ids, &node_contracts, &mut diagnostics);
        let endpoints =
            check_connection_endpoints(&self, &node_ids, &node_contracts, &mut diagnostics);
        let resolved = endpoints.resolved();
        check_required_inputs(&self, &node_contracts, &endpoints.writers, &mut diagnostics);
        check_feedback_kinds(&self, &node_contracts, &mut diagnostics);
        check_async_fan_out(&self, &node_contracts, &resolved, &mut diagnostics);
        // Cycle analysis keys vertices by node id; a duplicated id (already
        // an error) is analyzed as its first declaration.
        let order = check_cycles(&self, &resolved, &mut diagnostics);

        let islands = order
            .as_ref()
            .map(|order| stream_islands(&self, &node_contracts, &resolved, order));
        // Without islands (an illegal cycle), or with a connection left out
        // of the topology (unresolved: a typo, say), whether a feedback edge
        // sits on a cycle or keeps islands apart is unknown: say nothing.
        if let Some(islands) = &islands
            && resolved.iter().all(|ok| *ok)
        {
            check_useless_feedback(&self, &resolved, islands, &mut diagnostics);
        }
        if let Some(islands) = &islands {
            check_island_claims(&self, islands, &mut diagnostics);
        }
        for island in islands.iter().flat_map(|i| &i.merged) {
            diagnostics.push(Diagnostic::MergedIsland {
                nodes: island.clone(),
            });
        }

        // `order` (and with it `islands`) is None only when IllegalCycle
        // errors were pushed, so the arms are exhaustive without any panic
        // path. Without errors, every node id is unique and resolved, and
        // every connection resolved.
        let connection_ports: Option<Vec<(usize, usize)>> =
            endpoints.ports.iter().copied().collect();
        match (order, islands, connection_ports) {
            (Some(order), Some(islands), Some(ports)) if !diagnostics.has_errors() => {
                // Compact the table to its resolved contracts (cloned once
                // each) and point each node at its contract.
                let mut compact: HashMap<&ComponentRef, usize> = HashMap::new();
                let mut resolved_contracts: Vec<ComponentContract> = Vec::new();
                for contract in table.iter().flatten() {
                    compact.entry(&contract.id).or_insert_with(|| {
                        resolved_contracts.push((*contract).clone());
                        resolved_contracts.len() - 1
                    });
                }
                let mut node_index = HashMap::with_capacity(self.nodes.len());
                let mut node_contract = Vec::with_capacity(self.nodes.len());
                let mut node_island = Vec::with_capacity(self.nodes.len());
                for (index, node) in self.nodes.iter().enumerate() {
                    let contract = node_contracts
                        .get(&node.id)
                        .and_then(|contract| compact.get(&contract.id));
                    let island = islands.island_of.get(&node.id);
                    if let (Some(&contract), Some(&island)) = (contract, island) {
                        node_index.insert(node.id.clone(), index);
                        node_contract.push(contract);
                        node_island.push(island);
                    }
                }
                let connection_ends = self
                    .connections
                    .iter()
                    .zip(ports)
                    .filter_map(|(conn, (output, input))| {
                        Some(Ends {
                            from_node: *node_index.get(&conn.from.node)?,
                            output,
                            to_node: *node_index.get(&conn.to.node)?,
                            input,
                        })
                    })
                    .collect();
                let mut node_links = vec![Vec::new(); self.nodes.len()];
                for (position, link) in self.links.iter().enumerate() {
                    if let Some(&node) = node_index.get(&link.node) {
                        node_links[node].push(position);
                    }
                }
                Ok(CompiledGraph {
                    contracts: resolved_contracts,
                    node_index,
                    node_contract,
                    node_island,
                    connection_ends,
                    node_links,
                    graph: self,
                    order,
                    islands: islands.islands,
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
    use crate::component::Capability;
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

        fn link(mut self, id: &str, node: &str, import: &str) -> Self {
            self.builder = self
                .builder
                .link(id, node, import, cref("provider"), "demo:caps/clock");
            self
        }

        fn set_resource(
            mut self,
            node: &str,
            resource: &str,
            claim: ResourceClaim,
        ) -> Result<Self, String> {
            self.builder = self
                .builder
                .set_resource(node, resource, claim)
                .map_err(|e| e.to_string())?;
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
        let interfaces: Vec<String> = compiled
            .required_capabilities()
            .into_iter()
            .map(|c| c.interface)
            .collect();
        assert_eq!(
            interfaces,
            ["demo:caps/clock", "demo:caps/log"],
            "capabilities deduped across nodes"
        );
        assert!(compiled.warnings().is_empty());
        assert_eq!(compiled.graph().metadata.name, "valid");
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
            .add_component(source())
            .add_component(stream_source())
            .add_node("m", cref("mixed"))
            .add_node("v", cref("source"))
            .add_node("s", cref("stream-src"))
            .connect("cv", PortRef::new("v", "out"), PortRef::new("m", "v"))
            .connect("cs", PortRef::new("s", "out"), PortRef::new("m", "s"))
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

    /// `outputs { st: stream, status: value }`.
    fn stream_and_status() -> ComponentContract {
        contract(
            "src",
            vec![],
            vec![
                port("st", PortKind::Stream),
                port("status", PortKind::Value),
            ],
            &[],
        )
    }

    /// `inputs { st: stream, threshold: value }`.
    fn stream_with_threshold() -> ComponentContract {
        contract(
            "dst",
            vec![
                port("st", PortKind::Stream),
                port("threshold", PortKind::Value),
            ],
            vec![],
            &[],
        )
    }

    fn island_names(compiled: &CompiledGraph) -> Vec<Vec<&str>> {
        compiled
            .islands()
            .iter()
            .map(|island| island.iter().map(|n| n.as_str()).collect())
            .collect()
    }

    #[test]
    fn a_value_path_out_of_an_island_and_back_merges_the_islands() {
        let graph = builder("t")
            .add_component(stream_and_status())
            .add_component(stream_with_threshold())
            .add_component(pass_through())
            .add_node("s", cref("src"))
            .add_node("t", cref("dst"))
            .add_node("r", cref("pass"))
            .connect("st", PortRef::new("s", "st"), PortRef::new("t", "st"))
            .connect("out", PortRef::new("s", "status"), PortRef::new("r", "in"))
            .connect(
                "back",
                PortRef::new("r", "out"),
                PortRef::new("t", "threshold"),
            )
            .build();
        let compiled = graph.compile().expect("merging keeps the graph legal");
        assert_eq!(island_names(&compiled), vec![vec!["s", "r", "t"]]);
        let warnings: Vec<_> = compiled.warnings().iter().cloned().collect();
        assert_eq!(
            warnings,
            vec![Diagnostic::MergedIsland {
                nodes: vec!["s".into(), "r".into(), "t".into()],
            }]
        );
    }

    #[test]
    fn a_cycle_through_three_islands_merges_all_of_them() {
        let mut builder = builder("t")
            .add_component(stream_and_status())
            .add_component(stream_with_threshold())
            .add_component(source());
        for i in 1..=3 {
            builder = builder
                .add_node(format!("s{i}").as_str(), cref("src"))
                .add_node(format!("t{i}").as_str(), cref("dst"))
                .connect(
                    format!("st{i}").as_str(),
                    PortRef::new(format!("s{i}"), "st"),
                    PortRef::new(format!("t{i}"), "st"),
                );
        }
        let graph = builder
            .add_node("lone", cref("source"))
            .connect(
                "v12",
                PortRef::new("s1", "status"),
                PortRef::new("t2", "threshold"),
            )
            .connect(
                "v23",
                PortRef::new("s2", "status"),
                PortRef::new("t3", "threshold"),
            )
            .connect(
                "v31",
                PortRef::new("s3", "status"),
                PortRef::new("t1", "threshold"),
            )
            .build();
        let compiled = graph.compile().expect("valid graph");
        let islands = island_names(&compiled);
        assert_eq!(islands.len(), 2, "{islands:?}");
        let merged = islands
            .iter()
            .find(|i| i.len() == 6)
            .expect("one merged island");
        let mut sorted = merged.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ["s1", "s2", "s3", "t1", "t2", "t3"]);
        assert!(islands.contains(&vec!["lone"]));
        assert_eq!(compiled.warnings().len(), 1);
    }

    #[test]
    fn a_feedback_edge_keeps_islands_apart() {
        let graph = builder("t")
            .add_component(stream_and_status())
            .add_component(contract(
                "dst",
                vec![
                    port("st", PortKind::Stream),
                    port("threshold", PortKind::Value).optional(),
                ],
                vec![],
                &[],
            ))
            .add_component(pass_through())
            .add_node("s", cref("src"))
            .add_node("t", cref("dst"))
            .add_node("r", cref("pass"))
            .connect("st", PortRef::new("s", "st"), PortRef::new("t", "st"))
            .connect("out", PortRef::new("s", "status"), PortRef::new("r", "in"))
            .connect_feedback(
                "back",
                PortRef::new("r", "out"),
                PortRef::new("t", "threshold"),
            )
            .build();
        let compiled = graph.compile().expect("valid graph");
        assert_eq!(island_names(&compiled), vec![vec!["s", "t"], vec!["r"]]);
        assert!(
            compiled.warnings().is_empty(),
            "no merge, and the feedback edge is not useless: it keeps the islands apart: {:?}",
            compiled.warnings()
        );
    }

    #[test]
    fn a_hashed_node_never_binds_through_an_unhashed_entry_to_another_hash() {
        let mut resolved = source();
        resolved.id.content_hash = Some("aa".into());
        let mut pinned = cref("source");
        pinned.content_hash = Some("bb".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("n", pinned.clone())
            .build();
        assert_eq!(
            diags(graph.compile([resolved.clone()].as_slice())),
            vec![Diagnostic::UnknownComponent {
                node: "n".into(),
                component: pinned,
            }],
            "the entry resolved to `aa`, so a node pinned to `bb` matches nothing"
        );

        let mut same = cref("source");
        same.content_hash = Some("aa".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("n", same)
            .build();
        let compiled = graph
            .compile([resolved].as_slice())
            .expect("the hash agrees with the resolved contract");
        assert_eq!(
            compiled
                .contract_for(&NodeId::from("n"))
                .unwrap()
                .id
                .content_hash
                .as_deref(),
            Some("aa")
        );
    }

    #[test]
    fn entries_resolving_to_one_contract_are_not_ambiguous() {
        let mut hashed = source();
        hashed.id.content_hash = Some("aa".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_component(hashed.id.clone())
            .add_node("n", cref("source"))
            .build();
        graph
            .compile([hashed].as_slice())
            .expect("both entries are the same contract");
    }

    #[test]
    fn islands_are_in_topological_order() {
        let graph = builder("t")
            .add_component(stream_and_status())
            .add_component(stream_with_threshold())
            .add_component(source())
            .add_node("p", cref("src"))
            .add_node("c", cref("dst"))
            .add_node("u", cref("source"))
            .connect("st", PortRef::new("p", "st"), PortRef::new("c", "st"))
            .connect(
                "v",
                PortRef::new("u", "out"),
                PortRef::new("c", "threshold"),
            )
            .build();
        let compiled = graph.compile().expect("valid graph");
        assert_eq!(
            island_names(&compiled),
            vec![vec!["u"], vec!["p", "c"]],
            "`u` feeds the stream island, so it comes first"
        );
    }

    #[test]
    fn a_source_returning_another_revision_is_not_trusted() {
        let mut pinned = cref("source");
        pinned.content_hash = Some("dead".into());
        let mut other = source();
        other.id.content_hash = Some("ff".into());
        let contracts: HashMap<ComponentRef, ComponentContract> =
            HashMap::from([(pinned.clone(), other)]);
        let graph = Graph::builder("t")
            .add_component(pinned.clone())
            .add_node("s", pinned.clone())
            .build();
        assert_eq!(
            diags(graph.compile(&contracts)),
            vec![Diagnostic::ContractNotFound(pinned)]
        );

        let hashless = source();
        let mut key = cref("source");
        key.content_hash = Some("aa".into());
        let contracts: HashMap<ComponentRef, ComponentContract> =
            HashMap::from([(key.clone(), hashless)]);
        let graph = Graph::builder("t").add_component(key.clone()).build();
        assert_eq!(
            diags(graph.compile(&contracts)),
            vec![Diagnostic::ContractNotFound(key)],
            "a pinned entry needs a contract with exactly that hash"
        );
    }

    #[test]
    fn a_long_chain_compiles_without_deep_recursion() {
        let mut builder = builder("chain").add_component(pass_through());
        let n = 50_000;
        for i in 0..n {
            builder = builder.add_node(format!("n{i}").as_str(), cref("pass"));
        }
        for i in 1..n {
            builder = builder.connect(
                format!("c{i}").as_str(),
                PortRef::new(format!("n{}", i - 1), "out"),
                PortRef::new(format!("n{i}"), "in"),
            );
        }
        let graph = builder
            .add_component(source())
            .add_node("head", cref("source"))
            .connect("c0", PortRef::new("head", "out"), PortRef::new("n0", "in"))
            .build();
        let compiled = graph.compile().expect("valid graph");
        assert_eq!(compiled.islands().len(), n + 1);
    }

    #[test]
    fn duplicate_node_ids_get_no_feedback_warnings() {
        let graph = builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("a", cref("pass"))
            .connect_feedback("c1", PortRef::new("a", "out"), PortRef::new("a", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UselessFeedback { .. })),
            "{diags:?}"
        );
    }

    #[test]
    fn an_option_output_feeds_an_optional_input() {
        let graph = builder("t")
            .add_component(contract(
                "maybe",
                vec![],
                vec![PortDef::new("m", PortKind::Value, Type::option(Type::U32))],
                &[],
            ))
            .add_component(contract(
                "opt",
                vec![PortDef::new("m", PortKind::Value, Type::U32).optional()],
                vec![],
                &[],
            ))
            .add_component(contract(
                "plain",
                vec![PortDef::new("m", PortKind::Value, Type::U32)],
                vec![],
                &[],
            ))
            .add_node("p", cref("maybe"))
            .add_node("o", cref("opt"))
            .add_node("q", cref("maybe"))
            .add_node("r", cref("plain"))
            .connect("ok", PortRef::new("p", "m"), PortRef::new("o", "m"))
            .connect("bad", PortRef::new("q", "m"), PortRef::new("r", "m"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::TypeMismatch {
                conn: "bad".into(),
                from: "option<u32>".into(),
                to: "u32".into(),
            }],
            "only an optional input takes an option straight through"
        );
    }

    #[test]
    fn a_hash_map_source_follows_the_single_match_rule() {
        let mut aa = source();
        aa.id.content_hash = Some("aa".into());
        let mut bb = source();
        bb.id.content_hash = Some("bb".into());
        let (aa_id, bb_id) = (aa.id.clone(), bb.id.clone());
        let contracts: HashMap<ComponentRef, ComponentContract> =
            HashMap::from([(cref("source"), bb), (aa.id.clone(), aa)]);
        let graph = Graph::builder("t").add_component(cref("source")).build();
        match diags(graph.compile(&contracts)).as_slice() {
            [Diagnostic::AmbiguousContract { component, matches }] => {
                assert_eq!(component, &cref("source"));
                let mut matches = matches.clone();
                matches.sort();
                assert_eq!(
                    matches,
                    vec![aa_id, bb_id],
                    "two revisions match an unpinned entry, whatever key one is stored under"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_illegal_cycle_elsewhere_reports_no_useless_feedback() {
        let graph = builder("t")
            .add_component(stream_and_status())
            .add_component(contract(
                "dst",
                vec![
                    port("st", PortKind::Stream),
                    port("threshold", PortKind::Value).optional(),
                ],
                vec![],
                &[],
            ))
            .add_component(pass_through())
            .add_node("s", cref("src"))
            .add_node("t", cref("dst"))
            .add_node("r", cref("pass"))
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .connect("st", PortRef::new("s", "st"), PortRef::new("t", "st"))
            .connect("out", PortRef::new("s", "status"), PortRef::new("r", "in"))
            .connect_feedback(
                "back",
                PortRef::new("r", "out"),
                PortRef::new("t", "threshold"),
            )
            .connect("ab", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect("ba", PortRef::new("b", "out"), PortRef::new("a", "in"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::IllegalCycle {
                nodes: vec!["a".into(), "b".into()],
            }]
        );
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
        let warnings: Vec<_> = compiled.warnings().iter().cloned().collect();
        assert_eq!(
            warnings,
            vec![Diagnostic::FeedbackOnlyInput {
                port: PortRef::new("a", "in"),
                conn: "c2".into(),
            }],
            "`a.in` needs a first value"
        );
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
        assert!(
            compiled
                .warnings()
                .iter()
                .any(|d| d == &Diagnostic::UselessFeedback { conn: "c1".into() }),
            "{:?}",
            compiled.warnings()
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
                nodes: vec!["n".into()],
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
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_component(hashed.id.clone())
            .add_node("n", node_ref)
            .build();
        let compiled = graph
            .compile([hashed].as_slice())
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
    fn duplicate_node_ids_do_not_suppress_cycle_analysis() {
        let graph = builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .add_node("c", cref("pass"))
            .connect("bc", PortRef::new("b", "out"), PortRef::new("c", "in"))
            .connect("cb", PortRef::new("c", "out"), PortRef::new("b", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            diags.contains(&Diagnostic::DuplicateNodeId("a".into())),
            "{diags:?}"
        );
        assert!(
            diags.contains(&Diagnostic::IllegalCycle {
                nodes: vec!["b".into(), "c".into()],
            }),
            "an unrelated duplicate hides nothing: {diags:?}"
        );
    }

    #[test]
    fn an_unresolved_twin_of_a_connection_id_forms_no_cycle() {
        let graph = builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .connect("c1", PortRef::new("a", "out"), PortRef::new("b", "in"))
            .connect("c1", PortRef::new("b", "bogus"), PortRef::new("a", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::IllegalCycle { .. })),
            "{diags:?}"
        );
    }

    #[test]
    fn feedback_edges_that_keep_islands_apart_together_are_useful() {
        let graph = builder("t")
            .add_component(stream_and_status())
            .add_component(contract(
                "dst",
                vec![
                    port("st", PortKind::Stream),
                    port("threshold", PortKind::Value).optional(),
                ],
                vec![],
                &[],
            ))
            .add_component(contract(
                "opt-pass",
                vec![port("in", PortKind::Value).optional()],
                vec![port("out", PortKind::Value)],
                &[],
            ))
            .add_node("a", cref("src"))
            .add_node("a2", cref("dst"))
            .add_node("b", cref("opt-pass"))
            .add_node("c", cref("opt-pass"))
            .connect("st", PortRef::new("a", "st"), PortRef::new("a2", "st"))
            .connect("ab", PortRef::new("a", "status"), PortRef::new("b", "in"))
            .connect_feedback("f1", PortRef::new("b", "out"), PortRef::new("c", "in"))
            .connect_feedback(
                "f2",
                PortRef::new("c", "out"),
                PortRef::new("a2", "threshold"),
            )
            .build();
        let compiled = graph.compile().expect("valid graph");
        assert!(
            compiled.warnings().is_empty(),
            "unmarking both would merge the islands: {:?}",
            compiled.warnings()
        );
    }

    #[test]
    fn an_unhashed_entry_needs_a_single_revision() {
        let mut hashed = source();
        hashed.id.content_hash = Some("h1".into());
        let graph = Graph::builder("t").add_component(cref("source")).build();
        assert_eq!(
            diags(graph.compile([source(), hashed.clone()].as_slice())),
            vec![Diagnostic::AmbiguousContract {
                component: cref("source"),
                matches: vec![cref("source"), hashed.id],
            }],
            "a hashless contract with the exact id is one of two revisions"
        );
    }

    #[test]
    fn copies_of_one_contract_count_once_whatever_their_docs() {
        let mut documented = source();
        documented.docs = Some("Echoes x.".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("s", cref("source"))
            .build();
        graph
            .clone()
            .compile(&vec![source(), documented])
            .expect("one id, so one contract");
    }

    #[test]
    fn hashless_contracts_with_one_id_and_other_ports_are_ambiguous() {
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("s", cref("source"))
            .build();
        let mut other = source();
        other.outputs = vec![port("b", PortKind::Value)];
        for source_order in [vec![source(), other.clone()], vec![other.clone(), source()]] {
            let diagnostics = diags(graph.clone().compile(&source_order));
            assert!(
                matches!(
                    diagnostics.as_slice(),
                    [Diagnostic::AmbiguousContract { matches, .. }] if matches.len() == 2
                ),
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn a_pinned_node_never_binds_a_hashless_contract() {
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("s", {
                let mut pinned = cref("source");
                pinned.content_hash = Some("bb".into());
                pinned
            })
            .build();
        let diagnostics = diags(graph.compile(&vec![source()]));
        assert!(
            matches!(
                diagnostics.as_slice(),
                [Diagnostic::UnknownComponent { .. }]
            ),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn duplicated_or_missing_entries_add_no_ambiguity() {
        // [ghost, ghost] with no contracts: one ContractNotFound, one
        // DuplicateComponent, and no ambiguity.
        let graph = Graph::builder("t")
            .add_component(cref("ghost"))
            .add_component(cref("ghost"))
            .add_node("n", cref("ghost"))
            .build();
        assert_eq!(
            diags(graph.compile(&[] as &[ComponentContract])),
            vec![
                Diagnostic::ContractNotFound(cref("ghost")),
                Diagnostic::DuplicateComponent(cref("ghost")),
            ]
        );
        // [src (resolves to #aa), src#aa]: one contract.
        let mut aa = source();
        aa.id.content_hash = Some("aa".into());
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_component(aa.id.clone())
            .add_node("n", cref("source"))
            .build();
        graph.compile(&vec![aa]).expect("one contract");
    }

    #[test]
    fn hash_map_sources_resolve_deterministically() {
        let mut source_map: HashMap<ComponentRef, ComponentContract> = HashMap::new();
        for hash in ["dd", "aa", "cc", "bb"] {
            let mut c = source();
            c.id.content_hash = Some(hash.into());
            source_map.insert(c.id.clone(), c);
        }
        let graph = Graph::builder("t")
            .add_component(cref("source"))
            .add_node("s", cref("source"))
            .build();
        let diagnostics = diags(graph.compile(&source_map));
        let Some(Diagnostic::AmbiguousContract { matches, .. }) = diagnostics.first() else {
            panic!("{diagnostics:?}");
        };
        let hashes: Vec<&str> = matches
            .iter()
            .filter_map(|m| m.content_hash.as_deref())
            .collect();
        assert_eq!(hashes, ["aa", "bb", "cc", "dd"]);
    }

    #[test]
    fn a_typo_in_a_consumer_is_no_fan_out() {
        let producer = contract("producer", vec![], vec![port("out", PortKind::Stream)], &[]);
        let consumer = contract("consumer", vec![port("in", PortKind::Stream)], vec![], &[]);
        let graph = builder("t")
            .add_component(producer)
            .add_component(consumer)
            .add_node("p", cref("producer"))
            .add_node("c", cref("consumer"))
            .connect("st", PortRef::new("p", "out"), PortRef::new("c", "in"))
            .connect("typo", PortRef::new("p", "out"), PortRef::new("c", "inn"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::UnknownPort {
                conn: "typo".into(),
                port: PortRef::new("c", "inn"),
            }]
        );
    }

    #[test]
    fn an_island_claiming_more_than_a_resource_is_rejected() {
        let producer = contract("producer", vec![], vec![port("out", PortKind::Stream)], &[]);
        let consumer = contract("consumer", vec![port("in", PortKind::Stream)], vec![], &[]);
        let claim = || ResourceClaim::new(0.6).unwrap();
        let graph = builder("t")
            .add_component(producer)
            .add_component(consumer)
            .add_node("p", cref("producer"))
            .add_node("c", cref("consumer"))
            .connect("st", PortRef::new("p", "out"), PortRef::new("c", "in"))
            .set_resource("p", "gpu", claim())
            .unwrap()
            .set_resource("c", "gpu", claim())
            .unwrap()
            .build();
        let diagnostics = diags(graph.compile());
        assert_eq!(
            diagnostics,
            vec![Diagnostic::IslandOverclaims {
                nodes: vec!["p".into(), "c".into()],
                resource: "gpu".into(),
            }]
        );
        assert_eq!(
            diagnostics[0].location(),
            crate::Location::Island(vec!["p".into(), "c".into()])
        );
        // Apart, each island fits.
        let graph = builder("t")
            .add_component(source())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .connect("v", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .set_resource("s", "gpu", claim())
            .unwrap()
            .set_resource("d", "gpu", claim())
            .unwrap()
            .build();
        graph
            .compile()
            .expect("value-connected nodes are separate islands");
    }

    #[test]
    fn a_value_port_needs_a_payload_type() {
        let untyped = contract(
            "untyped",
            vec![],
            vec![PortDef::unit("out", PortKind::Value)],
            &[],
        );
        let graph = builder("t")
            .add_component(untyped)
            .add_node("u", cref("untyped"))
            .build();
        assert_eq!(
            diags(graph.compile()),
            vec![Diagnostic::UntypedValuePort {
                component: cref("untyped"),
                direction: PortDirection::Output,
                port: "out".into(),
            }]
        );
    }

    #[test]
    fn the_resolved_view_matches_the_graph() {
        let graph = builder("t")
            .add_component(source())
            .add_component(pass_through())
            .add_component(sink())
            .add_node("s", cref("source"))
            .add_node("p", cref("pass"))
            .add_node("d", cref("sink"))
            .connect("a", PortRef::new("s", "out"), PortRef::new("p", "in"))
            .connect("b", PortRef::new("p", "out"), PortRef::new("d", "in"))
            .build();
        let compiled = graph.compile().expect("valid graph");
        let nodes: Vec<(usize, &str, &str)> = compiled
            .nodes()
            .map(|n| (n.index, n.node.id.as_str(), n.contract.id.world.as_str()))
            .collect();
        assert_eq!(
            nodes,
            [(0, "s", "source"), (1, "p", "pass"), (2, "d", "sink")]
        );
        let connections: Vec<(&str, usize, &str, usize, &str, bool)> = compiled
            .connections()
            .map(|c| {
                (
                    c.connection.id.as_str(),
                    c.from_node,
                    c.from.name.as_str(),
                    c.to_node,
                    c.to.name.as_str(),
                    c.unwraps_option,
                )
            })
            .collect();
        assert_eq!(
            connections,
            [
                ("a", 0, "out", 1, "in", false),
                ("b", 1, "out", 2, "in", false)
            ]
        );
        let p = compiled.node(&NodeId::from("p")).expect("known node");
        assert_eq!(p.island, compiled.island_of(&NodeId::from("p")).unwrap());
        assert!(compiled.node(&NodeId::from("ghost")).is_none());
        let depth = compiled.depth_map();
        assert_eq!(
            (depth[&NodeId::from("s")], depth[&NodeId::from("d")]),
            (0, 2)
        );
    }

    #[test]
    fn arrays_and_indexes_are_contract_sources() {
        let graph = || {
            Graph::builder("t")
                .add_component(cref("source"))
                .add_node("s", cref("source"))
                .build()
        };
        graph().compile(&[source()]).expect("an array is a source");
        let index: ContractIndex = [source(), sink()].into_iter().collect();
        graph().compile(&index).expect("an index is a source");
        let mut other = source();
        other.id.content_hash = Some("x".into());
        let index = ContractIndex::new([source(), other]);
        assert!(matches!(
            diags(graph().compile(&index)).as_slice(),
            [Diagnostic::AmbiguousContract { .. }, ..]
        ));
    }

    #[test]
    fn writers_and_consumers_on_unknown_nodes_do_not_cascade() {
        let graph = builder("t")
            .add_component(source())
            .add_component(sink())
            .add_component(stream_source())
            .add_component(stream_sink())
            .add_node("s", cref("source"))
            .add_node("d", cref("sink"))
            .add_node("p", cref("stream-src"))
            .add_node("c1", cref("stream-sink"))
            .connect("real", PortRef::new("s", "out"), PortRef::new("d", "in"))
            .connect(
                "ghosted",
                PortRef::new("ghost", "out"),
                PortRef::new("d", "in"),
            )
            .connect("st", PortRef::new("p", "out"), PortRef::new("c1", "in"))
            .connect("typo", PortRef::new("p", "out"), PortRef::new("c1x", "in"))
            .connect(
                "loop",
                PortRef::new("nobody", "out"),
                PortRef::new("nobody", "in"),
            )
            .build();
        let diags = diags(graph.compile());
        assert!(
            !diags.iter().any(|d| matches!(
                d,
                Diagnostic::MultipleWriters { .. } | Diagnostic::AsyncFanOut { .. }
            )),
            "{diags:?}"
        );
        let unknown_nobody = diags
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnknownNode { node, .. } if node.as_str() == "nobody"),
            )
            .count();
        assert_eq!(
            unknown_nobody, 1,
            "one unknown node, reported once: {diags:?}"
        );
    }

    #[test]
    fn an_unresolved_edge_hides_useless_feedback_warnings() {
        let graph = builder("t")
            .add_component(pass_through())
            .add_node("a", cref("pass"))
            .add_node("b", cref("pass"))
            .connect("fwd", PortRef::new("a", "out"), PortRef::new("b", "inn"))
            .connect_feedback("back", PortRef::new("b", "out"), PortRef::new("a", "in"))
            .build();
        let diags = diags(graph.compile());
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UselessFeedback { .. })),
            "the typo hides the cycle `back` sits on: {diags:?}"
        );
    }

    #[test]
    fn many_revisions_of_one_world_compile_in_linear_time() {
        // n pinned revisions of one world (no contracts: every entry is
        // missing), and n nodes on the unhashed reference: one ambiguity,
        // reported once, whatever n.
        let compile = |n: usize| {
            let mut builder = Graph::builder("revisions");
            for i in 0..n {
                let mut id = cref("w");
                id.content_hash = Some(format!("{i:08x}"));
                builder = builder.add_component(id);
            }
            for i in 0..n {
                builder = builder.add_node(format!("n{i}").as_str(), cref("w"));
            }
            let started = std::time::Instant::now();
            let diagnostics = diags(builder.build().compile(&[] as &[ComponentContract]));
            let elapsed = started.elapsed();
            let ambiguous: Vec<_> = diagnostics
                .iter()
                .filter(|d| matches!(d, Diagnostic::AmbiguousComponent { .. }))
                .collect();
            assert_eq!(ambiguous.len(), 1);
            elapsed
        };
        compile(500);
        let small = compile(4_000);
        let large = compile(8_000);
        // Quadratic work would take about four times as long; allow a wide
        // margin for timing noise.
        assert!(
            large < small * 3 + std::time::Duration::from_millis(200),
            "{small:?} for 4k, {large:?} for 8k"
        );
    }

    #[test]
    fn a_large_table_compiles_in_linear_time() {
        // 10k distinct components, one node each, through an index: every
        // lookup and match is per world, so this stays fast.
        let n = 10_000;
        let contracts: Vec<ComponentContract> = (0..n)
            .map(|i| {
                contract(
                    &format!("w{i}"),
                    vec![],
                    vec![port("out", PortKind::Value)],
                    &[],
                )
            })
            .collect();
        let mut builder = Graph::builder("big");
        for (i, c) in contracts.iter().enumerate() {
            builder = builder
                .add_component(c)
                .add_node(format!("n{i}").as_str(), c.id.clone());
        }
        let index = ContractIndex::new(contracts);
        let started = std::time::Instant::now();
        builder.build().compile(&index).expect("valid graph");
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }

    #[test]
    fn required_capabilities_are_one_entry_per_link_name() {
        let with = |world: &str, cap: &str, items: &[(&str, &str)]| {
            let mut c = contract(world, vec![], vec![port("out", PortKind::Value)], &[]);
            let mut capability = Capability::new(cap);
            capability.items = items
                .iter()
                .map(|(n, s)| ((*n).to_string(), (*s).to_string()))
                .collect();
            c.capabilities = vec![capability];
            c
        };
        let graph = builder("t")
            .add_component(with("a", "config", &[("get", "func()->u32")]))
            .add_component(with("b", "config", &[("set", "func(\"v\":u32)")]))
            .add_component(with("c", "func:log", &[("log", "func(\"m\":string)")]))
            .add_component(with("d", "func:log", &[("log", "func(\"l\":u8)")]))
            .add_node("a", cref("a"))
            .add_node("b", cref("b"))
            .add_node("c", cref("c"))
            .add_node("d", cref("d"))
            .build();
        let required = graph
            .compile()
            .expect("valid graph")
            .required_capabilities();
        assert_eq!(required.len(), 2, "one entry per link name");
        let config = &required[0];
        assert_eq!(config.interface, "config");
        assert_eq!(config.items.len(), 2, "items merge");
        assert!(!config.conflicts());
        let log = &required[1];
        assert_eq!(log.interface, "func:log");
        assert_eq!(
            log.items["log"],
            BTreeSet::from([
                "func(\"l\":u8)".to_string(),
                "func(\"m\":string)".to_string()
            ]),
            "every signature is listed"
        );
        assert!(log.conflicts());
    }

    #[test]
    fn a_link_covers_the_imports_it_is_merged_with() {
        let mut c = contract("merged", vec![], vec![port("out", PortKind::Value)], &[]);
        let mut old = Capability::new("a:b/c@0.2.0");
        old.items = BTreeMap::from([("f".to_string(), "func()".to_string())]);
        let mut new = Capability::new("a:b/c@0.2.5");
        new.items = BTreeMap::from([("g".to_string(), "func()".to_string())]);
        let other = Capability::new("a:b/c@0.3.0");
        c.capabilities = vec![old, new, other];
        let required = builder("t")
            .add_component(c)
            .add_node("n", cref("merged"))
            .link("l", "n", "a:b/c@0.2.0")
            .build()
            .compile()
            .expect("valid graph")
            .required_capabilities();
        let names: Vec<&str> = required.iter().map(|r| r.interface.as_str()).collect();
        assert_eq!(names, ["a:b/c@0.3.0"], "0.2.5 merges with the linked 0.2.0");

        // Two links for what the component imports as one: refused.
        let mut c = contract("twice", vec![], vec![port("out", PortKind::Value)], &[]);
        c.capabilities = vec![
            Capability::new("a:b/c@0.2.0"),
            Capability::new("a:b/c@0.2.5"),
        ];
        let failure = builder("t")
            .add_component(c)
            .add_node("n", cref("twice"))
            .link("l1", "n", "a:b/c@0.2.0")
            .link("l2", "n", "a:b/c@0.2.5")
            .build()
            .compile()
            .expect_err("one merged import, two links");
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::ImportLinkedTwice { .. })),
            "{failure:?}"
        );
    }

    #[test]
    fn a_link_names_an_import_as_the_wit_declares_it_whatever_the_bytes_merged() {
        // Lowered from the bytes, where wit-component merged `@0.2.0` into
        // `@0.2.5`: a link written against the WIT's `@0.2.0` still names
        // that import.
        let mut c = contract("bytes", vec![], vec![port("out", PortKind::Value)], &[]);
        c.capabilities = vec![Capability::new("a:b/c@0.2.5")];
        let required = builder("t")
            .add_component(c)
            .add_node("n", cref("bytes"))
            .link("l", "n", "a:b/c@0.2.0")
            .build()
            .compile()
            .expect("the link names the merged import")
            .required_capabilities();
        assert!(required.is_empty(), "{required:?}");
    }

    #[test]
    fn one_label_for_two_interfaces_is_two_entries() {
        let with = |world: &str, implements: &str, item: &str| {
            let mut c = contract(world, vec![], vec![port("out", PortKind::Value)], &[]);
            let mut capability = Capability::new("primary");
            capability.implements = Some(implements.to_string());
            capability.items = BTreeMap::from([(item.to_string(), "func()".to_string())]);
            c.capabilities = vec![capability];
            c
        };
        let required = builder("t")
            .add_component(with("a", "demo:caps/kv", "get"))
            .add_component(with("b", "demo:caps/clock", "get"))
            .add_node("a", cref("a"))
            .add_node("b", cref("b"))
            .build()
            .compile()
            .expect("valid graph")
            .required_capabilities();
        let found: Vec<(&str, Option<&str>)> = required
            .iter()
            .map(|r| (r.interface.as_str(), r.implements.as_deref()))
            .collect();
        assert_eq!(
            found,
            [
                ("primary", Some("demo:caps/clock")),
                ("primary", Some("demo:caps/kv")),
            ]
        );
        assert!(required.iter().all(|r| !r.conflicts()));
    }

    #[test]
    fn a_linked_import_is_not_required_of_the_host() {
        let graph = builder("t")
            .add_component(source())
            .add_node("a", cref("source"))
            .add_node("b", cref("source"))
            .link("l", "a", "demo:caps/clock")
            .build();
        let compiled = graph.compile().expect("valid graph");
        let names = |required: Vec<RequiredCapability>| -> Vec<String> {
            required.into_iter().map(|r| r.interface).collect()
        };
        assert_eq!(
            names(compiled.required_capabilities()),
            ["demo:caps/clock", "demo:caps/log"],
            "`b` still imports the clock from the host"
        );
        let (a, b) = (NodeId::from("a"), NodeId::from("b"));
        assert_eq!(compiled.links_of(&a).count(), 1);
        assert_eq!(compiled.links_of(&b).count(), 0);

        let both = builder("t")
            .add_component(source())
            .add_node("a", cref("source"))
            .link("l", "a", "demo:caps/clock")
            .build()
            .compile()
            .expect("valid graph");
        assert_eq!(names(both.required_capabilities()), ["demo:caps/log"]);
    }

    #[test]
    fn links_are_checked() {
        let mut bare = contract("bare", vec![], vec![port("out", PortKind::Value)], &[]);
        bare.capabilities = vec![Capability::new("func:blink"), Capability::new("config")];
        let failure = builder("t")
            .add_component(source())
            .add_component(bare)
            .add_node("a", cref("source"))
            .add_node("b", cref("bare"))
            .link("ok", "a", "demo:caps/clock")
            .link("ok", "b", "config")
            .link("ghost", "nobody", "demo:caps/clock")
            .link("typo", "a", "demo:caps/clok")
            .link("func", "b", "func:blink")
            .link("again", "a", "demo:caps/clock")
            .build()
            .compile()
            .expect_err("bad links");
        let found: Vec<String> = failure
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            found,
            [
                "duplicate link id `ok`",
                "link `ghost` references unknown node `nobody`",
                "link `typo`: node `a` has no linkable import `demo:caps/clok`",
                "link `func`: node `b` has no linkable import `func:blink`",
                "link `again` links import `demo:caps/clock` of node `a`, which `ok` already links",
            ]
        );
        assert!(failure.diagnostics.iter().all(|d| {
            d.is_error() && matches!(d.location(), crate::diagnostics::Location::Link(_))
        }));
    }

    #[test]
    fn required_capabilities_do_not_depend_on_node_order() {
        let with = |world: &str, items: &[(&str, &str)]| {
            let mut c = contract(world, vec![], vec![port("out", PortKind::Value)], &[]);
            let mut capability = Capability::new("config");
            capability.items = items
                .iter()
                .map(|(n, s)| ((*n).to_string(), (*s).to_string()))
                .collect();
            c.capabilities = vec![capability];
            c
        };
        let required = |order: [&str; 3]| {
            let mut b = builder("t")
                .add_component(with("a", &[("f", "s1")]))
                .add_component(with("b", &[("g", "t1")]))
                .add_component(with("c", &[("g", "t2")]));
            for node in order {
                b = b.add_node(node, cref(node));
            }
            b.build()
                .compile()
                .expect("valid graph")
                .required_capabilities()
        };
        assert_eq!(required(["a", "b", "c"]), required(["c", "a", "b"]));
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
