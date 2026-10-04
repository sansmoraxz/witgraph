//! Loading: checking component bytes against their contracts, compiling
//! and linking them, and wiring the islands.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use wasmtime::Engine;
use wasmtime::component::{Component, Type};
use witgraph_ir::{
    Capability, CompiledConnection, CompiledGraph, ComponentContract, ComponentRef, NodeId,
    PortDef, PortKind, PortRef,
};
use witgraph_sched::{RunShape, Scheduler};
use witgraph_wit::lower::Lowered;

use super::compose::{self, LinkedProvider};
use super::{RuntimeConfig, RuntimeGraph};
use crate::engine::{
    self, BuildError, CapabilityGap, Host, NoCapabilities, NodeBinary, RunSignature, Wasmtime,
};
use crate::error::LoadError;
use witgraph_sched::{RuntimeError, RuntimeMode};

/// A component checked against its contract and compiled, ready for
/// [`RuntimeGraph::load_prepared`].
///
/// Preparing is the slow part of a load: it validates the bytes, decodes
/// and checks their WIT, and compiles them with Cranelift, which can take
/// seconds. It runs synchronously, so an embedder can run it off its async
/// executor (`spawn_blocking`, say) and keep the result to load the same
/// component again without recompiling. A prepared component belongs to
/// the engine it was compiled on, and to the contract it was checked
/// against.
#[derive(Clone)]
pub struct PreparedComponent {
    parts: Parts,
    component: Component,
}

/// What checking a component against its contract found: what a
/// [`PreparedComponent`] is besides its compiled code.
#[derive(Clone)]
struct Parts {
    contract: ComponentContract,
    /// The name the component exports its `node` interface under.
    export: String,
    /// The links composed in, sorted: import, provider, export.
    links: Vec<LinkKey>,
    /// What the component imports, by its own view of its world (with links
    /// composed in, what is left of the node's imports plus the
    /// providers'): what the host is asked for. A component imports only
    /// what its code uses, which can be fewer than its contract declares.
    capabilities: Vec<Capability>,
    /// How many components an instance holds: the node's, and one per
    /// provider composed in.
    components: usize,
}

/// What identifies a link among a node's: its import, provider and export.
type LinkKey = (String, ComponentRef, String);

/// A component checked against its contract, with its links composed in:
/// everything preparing does except compiling.
struct Checked<'a> {
    parts: Parts,
    /// The node's bytes, or the composed component's.
    bytes: Cow<'a, [u8]>,
}

impl<'a> Checked<'a> {
    /// Composes `links` into `bytes`, which implement `contract` as
    /// `lowered` (from [`verify`]) says.
    ///
    /// A link whose import the bytes do not have (their code never uses
    /// it) has nothing to satisfy and is left out; one the bytes import at
    /// a newer semver-compatible version (wit-component merges such
    /// imports) satisfies that import. `decodable` holds the providers
    /// (by the address and length of their bytes) already checked in this
    /// load.
    fn new(
        contract: &'a ComponentContract,
        bytes: &'a [u8],
        lowered: &Lowered,
        links: &[LinkedProvider<'_>],
        decodable: &mut HashSet<(usize, usize)>,
    ) -> Result<Self, LoadError> {
        let bad_link = |import: &str, message: String| LoadError::BadLink {
            component: Box::new(contract.id.clone()),
            import: import.to_string(),
            message,
        };
        let mut keys: Vec<LinkKey> = links
            .iter()
            .map(|l| {
                (
                    l.import.to_string(),
                    l.provider.clone(),
                    l.export.to_string(),
                )
            })
            .collect();
        keys.sort();
        // Each link with the name the bytes import it under.
        let mut used: Vec<(&LinkedProvider<'_>, &str)> = Vec::with_capacity(links.len());
        for link in links {
            let Some(argument) = imported_as(lowered, link.import) else {
                continue;
            };
            // Two contract imports wit-component merged into one take one
            // link, as compiling a graph requires (`ImportLinkedTwice`).
            if let Some((other, _)) = used.iter().find(|(_, name)| *name == argument) {
                return Err(bad_link(
                    link.import,
                    format!(
                        "the bytes import it and `{}` as one, `{argument}`, which another link \
                         already satisfies",
                        other.import
                    ),
                ));
            }
            // The provider's bytes are as untrusted as the node's, and the
            // composer decodes them: rule out what its decoder panics on.
            let key = (link.bytes.as_ptr() as usize, link.bytes.len());
            if decodable.insert(key) {
                witgraph_wit::verify::check_decodable(link.bytes).map_err(|e| {
                    decodable.remove(&key);
                    bad_link(link.import, format!("provider `{}`: {e}", link.provider))
                })?;
            }
            used.push((link, argument));
        }
        if used.is_empty() {
            return Ok(Self {
                parts: Parts {
                    contract: contract.clone(),
                    export: lowered.export.clone(),
                    links: keys,
                    capabilities: as_declared(contract, lowered, &lowered.contract.capabilities),
                    components: 1,
                },
                bytes: Cow::Borrowed(bytes),
            });
        }
        let export = lowered.export.clone();
        // What the check above does not rule out is still contained here
        // (where panics unwind).
        let composed = std::panic::catch_unwind(|| compose::compose(bytes, &export, &used))
            .map_err(|_| {
                let import = used.first().map_or("", |(link, _)| link.import);
                bad_link(import, "the composition could not decode a provider".into())
            })?
            .map_err(|(import, message)| bad_link(&import, message))?;
        // The composed component's own view of its world: its ports are the
        // node's, its imports what is left for the host. Lowering validates
        // the bytes first. The node's own bytes lowered, so what fails now,
        // a decoder panic included, came with the providers (what they
        // import).
        let with_providers = |message: String| {
            let import = used.first().map_or("", |(link, _)| link.import);
            bad_link(import, format!("with its providers composed in: {message}"))
        };
        let composed_view = std::panic::catch_unwind(|| {
            witgraph_wit::lower_component(&composed.bytes, &contract.id)
        })
        .map_err(|_| with_providers("the composed component's WIT could not be decoded".into()))?
        .map_err(|e| with_providers(e.to_string()))?;
        Ok(Self {
            parts: Parts {
                contract: contract.clone(),
                export,
                links: keys,
                capabilities: as_declared(contract, lowered, &composed_view.contract.capabilities),
                components: 1 + composed.providers,
            },
            bytes: Cow::Owned(composed.bytes),
        })
    }

    /// Compiles the component on `engine`, which must be able to run a
    /// graph, whichever way the component is prepared.
    fn compile(self, engine: &Engine) -> Result<PreparedComponent, LoadError> {
        validate_engine(engine)?;
        let component =
            Component::new(engine, &self.bytes).map_err(|e| LoadError::BadComponent {
                component: Box::new(self.parts.contract.id.clone()),
                message: format!("failed to compile: {e:#}"),
            })?;
        Ok(PreparedComponent {
            parts: self.parts,
            component,
        })
    }
}

impl PreparedComponent {
    /// Checks `bytes` against `contract` and compiles them on `engine`.
    ///
    /// The WIT embedded in the bytes is decoded and lowered, and must
    /// describe the same ports as `contract`, and import no
    /// capability item the contract does not declare with the same
    /// signature ([`LoadError::ContractMismatch`]). Bytes that are not a
    /// valid component, whose WIT cannot be decoded, or that fail to
    /// compile on `engine` are a [`LoadError::BadComponent`]; an engine
    /// that cannot run a graph is an `InvalidConfig`
    /// ([`LoadError::Runtime`]). What the bytes import is what the host is
    /// asked for when the graph loads ([`Host::check`], [`Host::link`]).
    pub fn new(
        engine: &Engine,
        contract: &ComponentContract,
        bytes: &[u8],
    ) -> Result<Self, LoadError> {
        Self::linked(engine, contract, bytes, &[])
    }

    /// Like [`new`](Self::new), for a node with links: checks `bytes`
    /// against `contract`, composes each link's provider into them, and
    /// compiles the result on `engine`.
    ///
    /// Composition checks every link it composes in: the provider must have
    /// the export, and the export must fit the import
    /// ([`LoadError::BadLink`]). A link whose import the bytes never use
    /// (their code never calls it) has nothing to satisfy: it is left out,
    /// and its provider is not looked at. Links naming the same provider,
    /// with the same bytes, share one instance. What
    /// the composed component still imports (the node's unlinked imports,
    /// and the providers' own) is what the host is asked for when the
    /// graph loads ([`Host::check`], [`Host::link`]).
    pub fn linked(
        engine: &Engine,
        contract: &ComponentContract,
        bytes: &[u8],
        links: &[LinkedProvider<'_>],
    ) -> Result<Self, LoadError> {
        let lowered = verify(contract, bytes)?;
        Checked::new(contract, bytes, &lowered, links, &mut HashSet::new())?.compile(engine)
    }

    /// The contract the component was checked against.
    pub fn contract(&self) -> &ComponentContract {
        &self.parts.contract
    }

    /// The engine the component was compiled on.
    pub fn engine(&self) -> &Engine {
        self.component.engine()
    }
}

/// The name component bytes (as `lowered` from them) import a contract's
/// `import` under: itself, or the newer semver-compatible version
/// wit-component merged it into. `None` when the bytes never import it.
fn imported_as<'l>(lowered: &'l Lowered, import: &str) -> Option<&'l str> {
    witgraph_ir::interface::resolve_import(&lowered.contract.capabilities, import)
        .filter(|c| c.is_interface())
        .map(|c| c.interface.as_str())
}

/// What the host is asked for and links: each capability in `imported`,
/// named as `contract` declares it when the node's own bytes (`lowered`)
/// import it under a newer semver-compatible version (wit-component
/// merges a dependency's newer pin into one import). Verification proved
/// every item the bytes use is in the declared version, so whatever serves
/// it serves them, and the linker resolves the newer import to it. What
/// only a composed provider imports is asked for as the provider imports
/// it.
fn as_declared(
    contract: &ComponentContract,
    lowered: &Lowered,
    imported: &[Capability],
) -> Vec<Capability> {
    let own = |name: &str| {
        lowered
            .contract
            .capabilities
            .iter()
            .any(|c| c.interface == name)
    };
    imported
        .iter()
        .map(|capability| {
            own(&capability.interface)
                .then(|| {
                    witgraph_ir::interface::resolve_import(
                        &contract.capabilities,
                        &capability.interface,
                    )
                })
                .flatten()
                .filter(|declared| declared.is_interface() == capability.is_interface())
                .unwrap_or(capability)
                .clone()
        })
        .collect()
}

/// Whether `ours`, a prepared component's links, are `theirs`, a node's: the
/// same imports and exports, from providers that match. A provider the
/// node pins by content hash must have been prepared under that very ref.
fn same_links(ours: &[LinkKey], theirs: &[LinkKey]) -> bool {
    ours.len() == theirs.len()
        && ours.iter().zip(theirs).all(|(ours, theirs)| {
            let provider = match theirs.1.content_hash {
                Some(_) => ours.1 == theirs.1,
                None => ours.1.matches(&theirs.1),
            };
            ours.0 == theirs.0 && ours.2 == theirs.2 && provider
        })
}

/// Asks `host` for every capability `node` imports.
fn check_host<H: Host>(
    host: &H,
    node: &NodeId,
    capabilities: &[Capability],
) -> Result<(), LoadError> {
    capabilities.iter().try_for_each(|capability| {
        host.check(node, capability)
            .map_err(|gap| capability_gap(node, capability, gap))
    })
}

impl<M: RuntimeMode> RuntimeGraph<M, NoCapabilities> {
    /// Loads a compiled graph whose components import no capabilities; see
    /// [`load_with_host`](RuntimeGraph::load_with_host).
    pub async fn load(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
    ) -> Result<Self, LoadError> {
        Self::load_with_host(compiled, wasm, config, mode, NoCapabilities).await
    }
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Loads a compiled graph with WASM component bytes, with `host`
    /// providing capability imports and island Store data.
    ///
    /// `wasm` maps each component a node instantiates (by the
    /// [`ComponentRef`] of its resolved contract, or the same ref without a
    /// content hash) to its encoded component bytes, and each provider a
    /// link names to its bytes. A provider has no contract for a content
    /// hash to name, so a link's provider pinned with one must be a key of
    /// `wasm` exactly; an unpinned one takes its own key, a key without a
    /// hash of its package, version and world, or the one pinned key of
    /// them. Each component is prepared
    /// ([`PreparedComponent::new`]) on [`RuntimeConfig::engine`] (or a new
    /// engine), then loaded as [`load_prepared`](Self::load_prepared) does.
    /// Every node's capabilities are checked ([`Host::check`]) before
    /// anything is compiled.
    ///
    /// Preparing compiles every component *synchronously on the task that
    /// polls this future*, which blocks its executor thread for as long as
    /// compiling takes (often seconds per component). On a shared async
    /// executor, prepare the components with [`PreparedComponent::new`] on
    /// a blocking thread instead, and call `load_prepared`.
    pub async fn load_with_host(
        compiled: CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        config: RuntimeConfig,
        mode: M,
        host: H,
    ) -> Result<Self, LoadError> {
        validate(&config)?;
        let engine = match &config.engine {
            Some(engine) => engine.clone(),
            None => config.new_engine()?,
        };
        validate_engine(&engine)?;
        let missing = |id: &ComponentRef| LoadError::MissingWasm {
            component: Box::new(id.clone()),
        };
        // One component per contract and set of links: nodes of one
        // component with other links are other compositions. Each
        // contract's bytes are verified once, whatever their links.
        let mut checked: Vec<Checked<'_>> = Vec::new();
        // Per contract: each set of links already checked, and where.
        let mut index_of: HashMap<&ComponentRef, Vec<(Vec<LinkKey>, usize)>> = HashMap::new();
        let mut verified: HashMap<&ComponentRef, (&[u8], Lowered)> = HashMap::new();
        let mut decodable = HashSet::new();
        let mut of_node = Vec::with_capacity(compiled.graph().nodes.len());
        for node in &compiled.graph().nodes {
            let contract = contract_of(&compiled, &node.id)?;
            let keys = link_keys(&compiled, &node.id);
            let known = index_of.entry(&contract.id).or_default();
            if let Some((_, index)) = known.iter().find(|(links, _)| *links == keys) {
                of_node.push((&node.id, *index));
                continue;
            }
            let (bytes, lowered) = match verified.entry(&contract.id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let bytes =
                        find_bytes(wasm, &contract.id).ok_or_else(|| missing(&contract.id))?;
                    entry.insert((bytes, verify(contract, bytes)?))
                }
            };
            // A link whose import the bytes never use is left out, so its
            // provider need not be there.
            let links = keys
                .iter()
                .map(|(import, provider, export)| {
                    let bytes = match imported_as(lowered, import) {
                        Some(_) => provider_bytes(wasm, provider)?,
                        None => &[][..],
                    };
                    Ok(LinkedProvider {
                        import,
                        provider,
                        bytes,
                        export,
                    })
                })
                .collect::<Result<Vec<_>, LoadError>>()?;
            checked.push(Checked::new(
                contract,
                bytes,
                lowered,
                &links,
                &mut decodable,
            )?);
            let index = checked.len() - 1;
            index_of
                .entry(&contract.id)
                .or_default()
                .push((keys, index));
            of_node.push((&node.id, index));
        }
        // Before the slow part: a host that cannot run the graph fails
        // fast.
        for &(node, index) in &of_node {
            check_host(&host, node, &checked[index].parts.capabilities)?;
        }
        let prepared = checked
            .into_iter()
            .map(|checked| checked.compile(&engine))
            .collect::<Result<Vec<_>, _>>()?;
        // Each node gets the very composition built (and checked) for it.
        let nodes = of_node.iter().map(|&(_, index)| &prepared[index]).collect();
        Self::link_and_build(compiled, nodes, config, engine, mode, host).await
    }

    /// Loads a compiled graph from prepared components (one per component a
    /// node instantiates, matched by its resolved contract), with `host`
    /// providing capability imports and island Store data.
    ///
    /// The engine is [`RuntimeConfig::engine`], or else the one the
    /// components were prepared on; every component must have been
    /// prepared on it ([`RuntimeError::InvalidConfig`]), against the very
    /// contract its nodes resolved to ([`LoadError::ContractMismatch`]).
    /// Every node's capabilities are checked ([`Host::check`]) before any
    /// is linked; then each node is linked once ([`Host::link`]), and every
    /// island is instantiated.
    pub async fn load_prepared(
        compiled: CompiledGraph,
        prepared: &[PreparedComponent],
        config: RuntimeConfig,
        mode: M,
        host: H,
    ) -> Result<Self, LoadError> {
        validate(&config)?;
        let engine = match (&config.engine, prepared.first()) {
            (Some(engine), _) => engine.clone(),
            (None, Some(first)) => first.engine().clone(),
            (None, None) => config.new_engine()?,
        };
        validate_engine(&engine)?;
        if let Some(other) = prepared.iter().find(|p| !Engine::same(p.engine(), &engine)) {
            return Err(LoadError::Runtime(RuntimeError::InvalidConfig {
                message: format!(
                    "`{}` was prepared on another engine",
                    other.parts.contract.id
                ),
            }));
        }
        // Every node's component, and what the host is asked for: all
        // checked before anything is linked.
        let mut by_id: HashMap<&ComponentRef, Vec<&PreparedComponent>> = HashMap::new();
        for p in prepared {
            by_id.entry(&p.parts.contract.id).or_default().push(p);
        }
        let mut nodes = Vec::with_capacity(compiled.graph().nodes.len());
        for node in &compiled.graph().nodes {
            let prepared = prepared_for(&compiled, &by_id, &node.id)?;
            check_host(&host, &node.id, &prepared.parts.capabilities)?;
            nodes.push(prepared);
        }
        Self::link_and_build(compiled, nodes, config, engine, mode, host).await
    }

    /// Links every node and builds every island, with a checked config and
    /// engine. `nodes` holds each graph node's component, in the graph's
    /// node order, its capabilities already checked ([`Host::check`]).
    async fn link_and_build(
        compiled: CompiledGraph,
        nodes: Vec<&PreparedComponent>,
        config: RuntimeConfig,
        engine: Engine,
        mode: M,
        host: H,
    ) -> Result<Self, LoadError> {
        let config = RuntimeConfig {
            engine: Some(engine.clone()),
            ..config
        };
        let settings = config.store_settings();
        // Before anything is instantiated, as the scheduler checks it last.
        witgraph_sched::check_resources(&compiled)?;

        let mut binaries = HashMap::new();
        for (node, prepared) in compiled.graph().nodes.iter().zip(nodes) {
            // The host links what the component imports.
            let contract = ComponentContract {
                capabilities: prepared.parts.capabilities.clone(),
                ..prepared.parts.contract.clone()
            };
            let instantiation = |e: wasmtime::Error| LoadError::Instantiation {
                node: node.id.clone(),
                message: format!("{e:#}"),
            };
            let linker = engine::node_linker(&engine, &node.id, &contract, &host)
                .map_err(|e| instantiation(e.context("linker")))?;
            let pre = linker
                .instantiate_pre(&prepared.component)
                .map_err(instantiation)?;
            binaries.insert(
                node.id.clone(),
                Arc::new(NodeBinary {
                    pre,
                    export: prepared.parts.export.clone(),
                    components: prepared.parts.components,
                }),
            );
        }

        let mut islands = Vec::new();
        let mut island_binaries = Vec::new();
        let mut island_cells = Vec::new();
        let mut input_types = HashMap::new();
        let mut output_types = HashMap::new();
        // Each island's own connections (both ends in it), found in one
        // pass over the graph.
        let island_of: Vec<usize> = compiled.nodes().map(|node| node.island).collect();
        let mut inner: Vec<Vec<CompiledConnection<'_>>> =
            vec![Vec::new(); compiled.islands().len()];
        for connection in compiled.connections() {
            let island = island_of.get(connection.from_node);
            if island == island_of.get(connection.to_node)
                && let Some(list) = island.and_then(|&i| inner.get_mut(i))
            {
                list.push(connection);
            }
        }
        for (members, connections) in compiled.islands().iter().zip(&inner) {
            let parts = island_parts(&binaries, members)?;
            let cells = cells(connections, members, config.split_islands);
            let data = engine::island_data(&host, config.max_island_memory, &parts, &cells)
                .into_iter()
                .zip(&cells)
                .map(|(data, cell)| {
                    data.map_err(|e| LoadError::Instantiation {
                        node: cell
                            .first()
                            .and_then(|member| members.get(*member).cloned())
                            .unwrap_or_else(|| "?".into()),
                        message: format!("island data: {e:#}"),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (island, signatures) =
                engine::build_island(&engine, &parts, &cells, data, settings)
                    .await
                    .map_err(
                        |BuildError { node, message, .. }| LoadError::Instantiation {
                            node,
                            message,
                        },
                    )?;
            let mut shapes = Vec::with_capacity(members.len());
            for (node, signature) in members.iter().zip(&signatures) {
                let contract = contract_of(&compiled, node)?;
                port_types(
                    node,
                    contract,
                    signature,
                    &mut input_types,
                    &mut output_types,
                );
                shapes.push(RunShape {
                    inputs: signature
                        .inputs
                        .as_ref()
                        .map(|fields| fields.iter().map(|(name, _)| name.clone()).collect()),
                    has_result: signature.has_result,
                });
            }
            island_binaries.push(
                parts
                    .iter()
                    .map(|(node, _)| ((*node).clone(), binaries[*node].clone()))
                    .collect(),
            );
            island_cells.push(cells);
            islands.push((island, shapes));
        }

        let executor = Wasmtime {
            host,
            engine,
            settings,
            max_island_memory: config.max_island_memory,
            binaries: island_binaries,
            cells: island_cells,
            input_types,
            output_types,
        };
        let sched = Scheduler::new(compiled, mode, executor, islands, config.max_steps_per_tick)?;
        Ok(Self { sched, config })
    }
}

/// Which members of an island share a Store, as positions in `members`:
/// members joined by a stream the host cannot pump between Stores
/// ([`crate::pump`]), or by a future, do; every other member has a Store
/// to itself. `connections` are the island's own. With `split` false, the
/// island is one Store. Cells are in the order of their first member.
fn cells(
    connections: &[CompiledConnection<'_>],
    members: &[NodeId],
    split: bool,
) -> Vec<Vec<usize>> {
    use witgraph_ir::wasm_wave::wasm::WasmType;

    if !split {
        return vec![(0..members.len()).collect()];
    }
    let pumped = |port: &PortDef| {
        port.kind == PortKind::Stream
            && port
                .ty
                .as_ref()
                .is_some_and(|ty| crate::pump::can_pump(ty.kind()))
    };
    let position: HashMap<&NodeId, usize> =
        members.iter().enumerate().map(|(i, n)| (n, i)).collect();
    let mut stores = witgraph_ir::partition::Partition::new(members.len());
    for resolved in connections {
        let conn = resolved.connection;
        let (Some(&from), Some(&to)) = (position.get(&conn.from.node), position.get(&conn.to.node))
        else {
            continue;
        };
        if conn.feedback || resolved.from.kind == PortKind::Value || pumped(resolved.from) {
            continue;
        }
        stores.union(from, to);
    }
    let mut cells: Vec<Vec<usize>> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    for member in 0..members.len() {
        let root = stores.find(member);
        let at = *index.entry(root).or_insert_with(|| {
            cells.push(Vec::new());
            cells.len() - 1
        });
        cells[at].push(member);
    }
    cells
}

/// Records the wasmtime type of every Value port of `node`: an input's
/// payload type (the inner type when optional), an output's type.
fn port_types(
    node: &NodeId,
    contract: &ComponentContract,
    signature: &RunSignature,
    input_types: &mut HashMap<PortRef, Type>,
    output_types: &mut HashMap<PortRef, Type>,
) {
    let value_port = |ports: &[PortDef], name: &str| {
        ports
            .iter()
            .find(|p| p.name.as_str() == name && p.kind == PortKind::Value)
            .cloned()
    };
    for (name, ty) in signature.inputs.iter().flatten() {
        if let Some(port) = value_port(&contract.inputs, name) {
            let payload = match (port.optional, ty) {
                (true, Type::Option(option)) => option.ty(),
                _ => ty.clone(),
            };
            input_types.insert(PortRef::new(node.clone(), port.name), payload);
        }
    }
    for (name, ty) in &signature.outputs {
        if let Some(port) = value_port(&contract.outputs, name) {
            output_types.insert(PortRef::new(node.clone(), port.name), ty.clone());
        }
    }
}

/// The links of `node`, as [`PreparedComponent`] keys them: sorted.
fn link_keys(compiled: &CompiledGraph, node: &NodeId) -> Vec<LinkKey> {
    let mut keys: Vec<LinkKey> = compiled
        .links_of(node)
        .map(|l| (l.import.clone(), l.provider.clone(), l.export.clone()))
        .collect();
    keys.sort();
    keys
}

/// The prepared component `node` runs: one prepared against its very
/// contract, with its links. A node whose link leaves its provider
/// unpinned takes the component prepared with exactly its links first, and
/// only otherwise one prepared with a pinned provider that matches.
fn prepared_for<'p>(
    compiled: &CompiledGraph,
    by_id: &HashMap<&ComponentRef, Vec<&'p PreparedComponent>>,
    node: &NodeId,
) -> Result<&'p PreparedComponent, LoadError> {
    let contract = contract_of(compiled, node)?;
    let keys = link_keys(compiled, node);
    let Some(same_id) = by_id.get(&contract.id) else {
        return Err(LoadError::MissingWasm {
            component: Box::new(contract.id.clone()),
        });
    };
    let mismatch = |message: String| LoadError::ContractMismatch {
        component: Box::new(contract.id.clone()),
        message,
    };
    let linked = || {
        same_id
            .iter()
            .copied()
            .filter(|p| same_links(&p.parts.links, &keys))
    };
    let fits = |p: &&PreparedComponent| p.parts.contract == *contract;
    let exact = linked().find(|p| fits(p) && p.parts.links == keys);
    match exact.or_else(|| linked().find(fits)) {
        Some(prepared) => Ok(prepared),
        None if linked().next().is_some() => Err(mismatch(
            "it was prepared against another contract with the same id".into(),
        )),
        None => Err(mismatch(format!(
            "it was not prepared with the links of node `{node}`"
        ))),
    }
}

/// The load error for a capability the host cannot provide to `node`.
fn capability_gap(node: &NodeId, capability: &Capability, gap: CapabilityGap) -> LoadError {
    let (node, implements) = (node.clone(), capability.implements.clone());
    let capability = capability.interface.clone();
    match gap {
        CapabilityGap::Missing => LoadError::MissingCapability {
            node,
            capability,
            implements,
        },
        CapabilityGap::Ambiguous(providers) => LoadError::AmbiguousCapability {
            node,
            capability,
            implements,
            providers,
        },
    }
}

pub(super) fn validate(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    let invalid = |message: &str| {
        Err(RuntimeError::InvalidConfig {
            message: message.into(),
        })
    };
    witgraph_sched::check_max_steps_per_tick(config.max_steps_per_tick)?;
    if config.yield_interval == 0 {
        return invalid("yield_interval must be at least 1");
    }
    if config.fuel_per_run == Some(0) {
        return invalid("fuel_per_run must be at least 1");
    }
    if config.max_island_memory == Some(0) {
        return invalid("max_island_memory must be at least 1");
    }
    if config.hostcall_fuel == 0 {
        return invalid("hostcall_fuel must be at least 1");
    }
    if let Some(bytes) = config.memory_reservation
        && bytes.checked_next_multiple_of(64 * 1024).is_none()
    {
        return invalid("memory_reservation is too large to round to whole 64 KiB pages");
    }
    Ok(())
}

/// Checks that an engine (maybe the embedder's) can run a graph: fuel
/// metering, the component model with its async ABI, concurrency, every
/// value type lowering admits in a capability signature (`map`,
/// `error-context`, fixed-length lists) and labelled interface imports; no
/// epoch interruption (no deadline
/// is ever set, so every call would trap); and no shared memories, whose
/// growth the island memory limit never sees.
fn validate_engine(engine: &Engine) -> Result<(), RuntimeError> {
    use wasmtime::WasmFeatures;
    let features = engine.get_wasm_features();
    let missing = [
        (engine.get_consume_fuel(), "fuel metering (`consume_fuel`)"),
        (
            engine.get_concurrency_support(),
            "concurrency (`concurrency_support`)",
        ),
        (
            features.contains(WasmFeatures::COMPONENT_MODEL),
            "the component model",
        ),
        (
            features.contains(WasmFeatures::CM_ASYNC),
            "the component model's async ABI",
        ),
        (
            features.contains(WasmFeatures::CM_MAP),
            "the component model's `map` type",
        ),
        (
            features.contains(WasmFeatures::CM_ERROR_CONTEXT),
            "the component model's `error-context` type",
        ),
        (
            features.contains(WasmFeatures::CM_FIXED_LENGTH_LISTS),
            "the component model's fixed-length lists",
        ),
        (
            features.contains(WasmFeatures::CM_IMPLEMENTS),
            "the component model's labelled imports (`implements`)",
        ),
        (
            !engine.get_epoch_interruption(),
            "no epoch interruption (witgraph sets no deadline)",
        ),
        (
            !engine.get_shared_memory(),
            "no shared memories (the island memory limit cannot see their growth)",
        ),
    ];
    match missing.iter().find(|(ok, _)| !ok) {
        Some((_, what)) => Err(RuntimeError::InvalidConfig {
            message: format!("the engine lacks {what}; use `RuntimeConfig::new_engine`"),
        }),
        None => Ok(()),
    }
}

pub(super) fn contract_of<'a>(
    compiled: &'a CompiledGraph,
    node: &NodeId,
) -> Result<&'a ComponentContract, RuntimeError> {
    compiled
        .contract_for(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
}

/// The bytes of a link's provider. A provider has no contract for a hash
/// to name, so a pinned ref takes only its own key. An unpinned one takes
/// its own key, else the one pinned key of its world: several are
/// [`LoadError::AmbiguousProvider`],
/// none [`LoadError::MissingWasm`].
fn provider_bytes<'a>(
    wasm: &HashMap<ComponentRef, &'a [u8]>,
    provider: &ComponentRef,
) -> Result<&'a [u8], LoadError> {
    let missing = || LoadError::MissingWasm {
        component: Box::new(provider.clone()),
    };
    if provider.content_hash.is_some() {
        return wasm.get(provider).copied().ok_or_else(missing);
    }
    // Unpinned: its own key, else the one pinned key of its world.
    if let Some(bytes) = wasm.get(provider) {
        return Ok(bytes);
    }
    let mut pinned: Vec<(&ComponentRef, &[u8])> = wasm
        .iter()
        .filter(|(key, _)| key.matches(provider))
        .map(|(key, bytes)| (key, *bytes))
        .collect();
    match pinned.len() {
        0 => Err(missing()),
        1 => Ok(pinned.remove(0).1),
        _ => {
            let mut candidates: Vec<ComponentRef> =
                pinned.into_iter().map(|(key, _)| key.clone()).collect();
            candidates.sort();
            Err(LoadError::AmbiguousProvider {
                provider: Box::new(provider.clone()),
                candidates,
            })
        }
    }
}

/// The bytes for a component: keyed by its full ref, or by a ref without
/// a content hash that matches it (same package, version included, and
/// world).
fn find_bytes<'a>(wasm: &HashMap<ComponentRef, &'a [u8]>, id: &ComponentRef) -> Option<&'a [u8]> {
    wasm.get(id).copied().or_else(|| {
        wasm.iter()
            .find(|(key, _)| key.content_hash.is_none() && key.matches(id))
            .map(|(_, bytes)| *bytes)
    })
}

/// Checks that `bytes` implement `contract`
/// ([`witgraph_wit::verify::verify_component`]), and returns the bytes' own
/// view of their world.
fn verify(contract: &ComponentContract, bytes: &[u8]) -> Result<Lowered, LoadError> {
    use witgraph_wit::verify::VerifyError;
    let component = Box::new(contract.id.clone());
    match witgraph_wit::verify::verify_component(contract, bytes) {
        Ok(lowered) => Ok(lowered),
        Err(VerifyError::Bad(message)) => Err(LoadError::BadComponent { component, message }),
        Err(VerifyError::Mismatch(message)) => {
            Err(LoadError::ContractMismatch { component, message })
        }
    }
}

/// Pairs each member of an island with what instantiating it needs, in
/// island order.
pub(super) fn island_parts<'a, D>(
    binaries: &'a HashMap<NodeId, Arc<NodeBinary<D>>>,
    members: &'a [NodeId],
) -> Result<Vec<(&'a NodeId, &'a NodeBinary<D>)>, RuntimeError> {
    members
        .iter()
        .map(|node| {
            binaries
                .get(node)
                .map(|binary| (node, &**binary))
                .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
        })
        .collect()
}
