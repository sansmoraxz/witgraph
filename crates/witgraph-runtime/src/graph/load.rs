//! Loading: checking component bytes against their contracts, composing them
//! with a generated hub, and compiling the result.

#[cfg(feature = "precompiled")]
mod precompiled;

#[cfg(feature = "compile")]
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
#[cfg(feature = "compile")]
use std::sync::OnceLock;

use wasmtime::Engine;
use wasmtime::component::Component;
#[cfg(feature = "compile")]
use witgraph_ir::ComponentContract;
#[cfg(feature = "compile")]
use witgraph_ir::PackageRef;
use witgraph_ir::{Capability, CompiledGraph, ComponentRef, IndexVec, NodeId, PortKind, PortTable};
#[cfg(feature = "compile")]
use witgraph_wit::compose::{LinkedProvider, imported_as, link_node};
#[cfg(feature = "compile")]
use witgraph_wit::hub::{self, HubMember};
#[cfg(feature = "compile")]
use witgraph_wit::lower::Lowered;

use super::{ByteCount, Fuel, GraphId, PortTypes, RuntimeConfig, RuntimeGraph};
#[cfg(feature = "compile")]
use crate::engine::NoCapabilities;
use crate::engine::{self, CapabilityGap, Host};
use crate::error::LoadError;
use crate::plane::{Blueprint, BuildFailure, Built, HubExports, HubPlane, build, store_data};
use crate::shared::SharedMemory;
use witgraph_ir::interface::compat_track;
use witgraph_sched::{RuntimeError, RuntimeMode, SchedulerCore};

/// A graph composed into one component with its hub and compiled, ready for
/// [`RuntimeGraph::load_prepared`].
///
/// Preparing is the slow part of a load: it validates the bytes, decodes and
/// checks their WIT, generates the hub, composes it with the nodes, and compiles
/// the result with Cranelift, which can take seconds. It runs synchronously, so an
/// embedder can run it off its async executor (`spawn_blocking`, say) and keep the
/// result to load the same graph again without recompiling. A prepared graph
/// belongs to the engine it was compiled on, and to the graph it was prepared for:
/// a change to any connection is another graph. With the `precompiled` feature it
/// can be saved, and loaded in another process without compiling, by a build with
/// no compiler in it.
#[derive(Clone)]
pub struct PreparedGraph {
    parts: Parts,
    component: Component,
}

/// What a [`PreparedGraph`] is besides its compiled code.
#[derive(Clone)]
struct Parts {
    /// What tells the graph it was prepared for from any other.
    graph: String,
    /// What each node imports, by its component's own view of its world (with
    /// links composed in, what is left of the node's imports plus the
    /// providers'): what the host is asked for. A component imports only what its
    /// code uses, which can be fewer than its contract declares.
    capabilities: Vec<(NodeId, Vec<Capability>)>,
    /// How many components an instance holds: the hub, each node, and each
    /// provider composed in.
    components: usize,
    /// A hash of the bytes and links it was prepared from, when they were
    /// given: what tells a saved graph from one rebuilt with the same contracts.
    #[cfg(feature = "precompiled")]
    source: Option<String>,
}

/// What identifies a link among a node's: its import, provider and export.
type LinkKey = (String, ComponentRef, String);

/// What tells `compiled` from any other graph: its nodes, what they run, and how
/// they are wired. Anything here changes the composite.
fn fingerprint(compiled: &CompiledGraph) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for node in compiled.topological_order() {
        let hash = compiled
            .contract_for(node)
            .map(witgraph_wit::hash::content_hash)
            .unwrap_or_default();
        let _ = writeln!(text, "node {node} {hash}");
        for (import, provider, export) in link_keys(compiled, node) {
            let _ = writeln!(text, "link {node} {import} {provider} {export}");
        }
    }
    let mut connections: Vec<String> = compiled
        .graph()
        .connections
        .iter()
        .map(|c| {
            format!(
                "connection {} {}.{} {}.{} {}",
                c.id, c.from.node, c.from.port, c.to.node, c.to.port, c.feedback
            )
        })
        .collect();
    connections.sort();
    text.push_str(&connections.join("\n"));
    text
}

/// A node's component, checked against its contract, with its links composed in.
#[cfg(feature = "compile")]
struct CheckedNode<'a> {
    export: String,
    capabilities: Vec<Capability>,
    components: usize,
    /// The node's bytes, or the composed component's.
    bytes: Cow<'a, [u8]>,
}

#[cfg(feature = "compile")]
impl PreparedGraph {
    /// Checks `wasm` against `compiled`'s contracts, composes it, and compiles
    /// it on `engine`.
    ///
    /// `wasm` maps each component a node instantiates (by the [`ComponentRef`] of
    /// its resolved contract, or the same ref without a content hash) to its
    /// encoded component bytes, and each provider a link names to its bytes. A
    /// provider has no contract for a content hash to name, so a link's provider
    /// pinned with one must be a key of `wasm` exactly; an unpinned one takes its
    /// own key, a key without a hash of its package, version and world, or the one
    /// pinned key of them.
    ///
    /// The WIT embedded in the bytes is decoded and lowered, and must describe
    /// the same ports as the contract, and import no capability item the contract
    /// does not declare with the same signature ([`LoadError::ContractMismatch`]).
    /// Bytes that are not a valid component, whose WIT cannot be decoded, or that
    /// fail to compile on `engine` are a [`LoadError::BadComponent`]; a graph that
    /// cannot be composed is a [`LoadError::Composition`]; an engine that cannot
    /// run a graph is an `InvalidConfig` ([`LoadError::Runtime`]). What the bytes
    /// import is what the host is asked for when the graph loads ([`Host::check`],
    /// [`Host::link`]).
    pub fn new(
        engine: &Engine,
        compiled: &CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
    ) -> Result<Self, LoadError> {
        Self::checked(engine, compiled, wasm, |_, _| Ok(()))
    }

    /// As [`new`](Self::new), asking `check` about each node's imports before
    /// anything is composed or compiled.
    fn checked(
        engine: &Engine,
        compiled: &CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        check: impl Fn(&NodeId, &[Capability]) -> Result<(), LoadError>,
    ) -> Result<Self, LoadError> {
        validate_engine(engine)?;
        let index = WasmIndex::new(wasm);
        let missing = |id: &ComponentRef| LoadError::MissingWasm {
            component: Box::new(id.clone()),
        };
        // One component per contract and set of links: nodes of one component
        // with other links are other compositions. Each contract's bytes are
        // verified once, whatever their links.
        let mut checked: Vec<CheckedNode<'_>> = Vec::new();
        let mut index_of: HashMap<(&ComponentRef, Vec<LinkKey>), usize> = HashMap::new();
        let mut verified: HashMap<&ComponentRef, (&[u8], Lowered)> = HashMap::new();
        let mut of_node = Vec::with_capacity(compiled.graph().nodes.len());
        for node in &compiled.graph().nodes {
            let contract = contract_of(compiled, &node.id)?;
            let key = (&contract.id, link_keys(compiled, &node.id));
            if let Some(&at) = index_of.get(&key) {
                of_node.push((&node.id, at));
                continue;
            }
            let (bytes, lowered) = match verified.entry(&contract.id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let bytes = index
                        .component(&contract.id)
                        .ok_or_else(|| missing(&contract.id))?;
                    entry.insert((bytes, verify(contract, bytes)?))
                }
            };
            // A link whose import the bytes never use is left out, so its
            // provider need not be there.
            let links = key
                .1
                .iter()
                .map(|(import, provider, export)| {
                    let bytes = match imported_as(lowered, import) {
                        Some(_) => index.provider(provider)?,
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
            let linked =
                link_node(contract, bytes, lowered, &links).map_err(|e| LoadError::BadLink {
                    component: Box::new(contract.id.clone()),
                    import: e.import,
                    message: e.message,
                })?;
            checked.push(CheckedNode {
                export: linked.export,
                capabilities: linked.capabilities,
                components: linked.components,
                bytes: linked.bytes,
            });
            index_of.insert(key, checked.len() - 1);
            of_node.push((&node.id, checked.len() - 1));
        }
        // Before the slow part: a host that cannot run the graph fails fast.
        for &(node, at) in &of_node {
            check(node, &checked[at].capabilities)?;
        }
        let members: Vec<HubMember<'_>> = of_node
            .iter()
            .map(|&(node, at)| HubMember {
                node,
                bytes: &checked[at].bytes,
                export: &checked[at].export,
                mocked: false,
            })
            .collect();
        let composed =
            hub::compose_graph(compiled, &members).map_err(|e| LoadError::Composition {
                message: e.to_string(),
            })?;
        let component =
            Component::new(engine, &composed.bytes).map_err(|e| LoadError::Composition {
                message: format!("failed to compile: {e:#}"),
            })?;
        let providers: usize = of_node
            .iter()
            .map(|&(_, at)| checked[at].components.saturating_sub(1))
            .sum();
        Ok(Self {
            parts: Parts {
                graph: fingerprint(compiled),
                capabilities: of_node
                    .iter()
                    .map(|&(node, at)| (node.clone(), checked[at].capabilities.clone()))
                    .collect(),
                components: composed.components + providers,
                #[cfg(feature = "precompiled")]
                source: Some(precompiled::source_hash(wasm)),
            },
            component,
        })
    }
}

impl PreparedGraph {
    /// The engine the graph was compiled on.
    pub fn engine(&self) -> &Engine {
        self.component.engine()
    }
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

/// Asks `host` whether it shares every resource a handle port of the
/// graph carries ([`Host::shares`]), and checks that no two of them have
/// one type tag ([`crate::shared`]).
fn check_shared<H: Host>(host: &H, compiled: &CompiledGraph) -> Result<(), LoadError> {
    let mut tags: HashMap<u32, (String, &str)> = HashMap::new();
    for node in &compiled.graph().nodes {
        let Some(contract) = compiled.contract_for(&node.id) else {
            continue;
        };
        for port in contract.inputs.iter().chain(&contract.outputs) {
            let Some(handle) = &port.handle else {
                continue;
            };
            if !host.shares(&node.id, handle) {
                return Err(LoadError::NotShareable {
                    node: node.id.clone(),
                    port: port.name.clone(),
                    resource: handle.to_string(),
                });
            }
            let resource = (compat_track(&handle.interface), handle.resource.as_str());
            let tag = crate::shared::tag(&handle.interface, &handle.resource);
            match tags.entry(tag) {
                Entry::Occupied(other) if *other.get() != resource => {
                    return Err(LoadError::Runtime(RuntimeError::InvalidConfig {
                        message: format!(
                            "resources `{handle}` and `{}.{}` have the same type tag; rename one",
                            other.get().0,
                            other.get().1
                        ),
                    }));
                }
                Entry::Occupied(_) => {}
                Entry::Vacant(entry) => {
                    entry.insert(resource);
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "compile")]
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
    /// Loads a compiled graph with WASM component bytes, with `host` providing
    /// capability imports and the Store's data.
    ///
    /// The graph is prepared ([`PreparedGraph::new`], which says what `wasm`
    /// holds) on [`RuntimeConfig::engine`] (or a new engine), then loaded as
    /// [`load_prepared`](Self::load_prepared) does. Every node's capabilities are
    /// checked ([`Host::check`]) before anything is compiled.
    ///
    /// Preparing compiles *synchronously on the task that polls this future*,
    /// which blocks its executor thread for as long as compiling takes (often
    /// seconds). On a shared async executor, prepare the graph with
    /// [`PreparedGraph::new`] on a blocking thread instead, and call `load_prepared`.
    #[cfg(feature = "compile")]
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
        check_shared(&host, &compiled)?;
        let prepared = PreparedGraph::checked(&engine, &compiled, wasm, |node, capabilities| {
            check_host(&host, node, capabilities)
        })?;
        Self::link_and_build(compiled, &prepared, config, engine, mode, host).await
    }

    /// Loads a compiled graph from a prepared one, with `host` providing
    /// capability imports and the Store's data.
    ///
    /// The engine is [`RuntimeConfig::engine`], or else the one the graph was
    /// prepared on; it must have been prepared on it
    /// ([`RuntimeError::InvalidConfig`]), and for this very graph
    /// ([`LoadError::GraphMismatch`]). Every node's capabilities are checked
    /// ([`Host::check`]) before any is linked; then the graph is linked
    /// ([`Host::link`]) and instantiated.
    pub async fn load_prepared(
        compiled: CompiledGraph,
        prepared: &PreparedGraph,
        config: RuntimeConfig,
        mode: M,
        host: H,
    ) -> Result<Self, LoadError> {
        validate(&config)?;
        let engine = config
            .engine
            .clone()
            .unwrap_or_else(|| prepared.engine().clone());
        validate_engine(&engine)?;
        if !Engine::same(prepared.engine(), &engine) {
            return Err(LoadError::Runtime(RuntimeError::InvalidConfig {
                message: "the graph was prepared on another engine".into(),
            }));
        }
        if prepared.parts.graph != fingerprint(&compiled) {
            return Err(LoadError::GraphMismatch {
                message: "it was prepared for another graph".into(),
            });
        }
        for (node, capabilities) in &prepared.parts.capabilities {
            check_host(&host, node, capabilities)?;
        }
        check_shared(&host, &compiled)?;
        Self::link_and_build(compiled, prepared, config, engine, mode, host).await
    }

    /// Links the graph and builds its first instance, with a checked config and
    /// engine, its capabilities already checked ([`Host::check`]).
    async fn link_and_build(
        compiled: CompiledGraph,
        prepared: &PreparedGraph,
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
        witgraph_sched::check_max_steps_per_tick(config.max_steps_per_tick)?;

        let instantiation = |message: String| LoadError::Instantiation {
            node: None,
            message,
        };
        let ports = Arc::new(PortTable::new(&compiled));
        // Nodes importing one name share one import.
        let mut capabilities: Vec<Capability> = Vec::new();
        for capability in prepared.parts.capabilities.iter().flat_map(|(_, c)| c) {
            if !capabilities
                .iter()
                .any(|c| c.link_name() == capability.link_name())
            {
                capabilities.push(capability.clone());
            }
        }
        let linker = engine::graph_linker(&engine, &ports, &capabilities, &host)
            .map_err(|e| instantiation(format!("{:#}", e.context("linker"))))?;
        let pre = linker
            .instantiate_pre(&prepared.component)
            .map_err(|e| instantiation(format!("{e:#}")))?;
        let exports = HubExports::new(&prepared.component, &ports)
            .map(Arc::new)
            .map_err(|e| instantiation(format!("{e:#}")))?;

        let shared_memory = SharedMemory::new(config.max_shared_memory.map(ByteCount::to_usize));
        let max_memory = config.max_memory.map(ByteCount::to_usize);
        let components = prepared.parts.components;
        let data = store_data(&host, max_memory, &shared_memory, components)
            .map_err(|e| instantiation(format!("store data: {e:#}")))?;
        let mut first: Built<H::Data> = build(&engine, &pre, &exports, data, settings)
            .await
            .map_err(|BuildFailure { message, fatal }| LoadError::Instantiation {
                node: fatal.map(|(node, _)| node),
                message,
            })?;
        let types = Arc::new(port_types(&mut first, &exports, &ports));

        let mode = Arc::new(mode);
        let blueprint = Blueprint {
            host,
            engine,
            pre,
            exports,
            settings,
            max_memory,
            components,
            shared_memory,
        };
        let core = SchedulerCore::new(
            compiled,
            mode.clone(),
            config.max_steps_per_tick,
            |_, ports| HubPlane::new(blueprint, ports.clone(), first),
        )?;
        Ok(Self {
            mode,
            core,
            types,
            config,
            id: GraphId::fresh(),
        })
    }
}

/// The type of every Value port, from what its getter returns: an input's payload
/// type (the inner type when optional), an output's type.
fn port_types<D: engine::StoreData>(
    built: &mut Built<D>,
    exports: &HubExports,
    ports: &PortTable,
) -> PortTypes {
    let Built { store, hub } = built;
    let inputs = IndexVec::from_fn(ports.inputs().len(), |input| {
        if ports.inputs()[input].kind != PortKind::Value {
            return None;
        }
        hub.getter_type(store, exports.input_getter(input))
    });
    let outputs = IndexVec::from_fn(ports.outputs().len(), |output| {
        if ports.outputs()[output].kind != PortKind::Value {
            return None;
        }
        hub.getter_type(store, exports.output_getter(output))
    });
    PortTypes { inputs, outputs }
}

/// The links of `node`, sorted.
fn link_keys(compiled: &CompiledGraph, node: &NodeId) -> Vec<LinkKey> {
    let mut keys: Vec<LinkKey> = compiled
        .links_of(node)
        .map(|l| (l.import.clone(), l.provider.clone(), l.export.clone()))
        .collect();
    keys.sort();
    keys
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
    if config.yield_interval.0 == 0 {
        return invalid("yield_interval must be at least 1");
    }
    if config.fuel_per_run == Some(Fuel(0)) {
        return invalid("fuel_per_run must be at least 1");
    }
    if config.max_memory == Some(ByteCount(0)) {
        return invalid("max_memory must be at least 1");
    }
    if config.max_shared_memory == Some(ByteCount(0)) {
        return invalid("max_shared_memory must be at least 1");
    }
    if config.hostcall_fuel == 0 {
        return invalid("hostcall_fuel must be at least 1");
    }
    if let Some(bytes) = config.memory_reservation
        && bytes.0.checked_next_multiple_of(64 * 1024).is_none()
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
/// growth the memory limit never sees.
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
            "no shared memories (the memory limit cannot see their growth)",
        ),
    ];
    match missing.iter().find(|(ok, _)| !ok) {
        Some((_, what)) => Err(RuntimeError::InvalidConfig {
            message: format!("the engine lacks {what}; use `RuntimeConfig::new_engine`"),
        }),
        None => Ok(()),
    }
}

#[cfg(feature = "compile")]
pub(super) fn contract_of<'a>(
    compiled: &'a CompiledGraph,
    node: &NodeId,
) -> Result<&'a ComponentContract, RuntimeError> {
    compiled
        .contract_for(node)
        .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })
}

/// The keys given for one world, with their bytes.
#[cfg(feature = "compile")]
type World<'w, 'a> = Vec<(&'w ComponentRef, &'a [u8])>;

/// The given bytes, looked up by exact key and, on a miss, by package
/// (version included) and world: a miss costs the keys of that one world,
/// not every key given. The index for misses is built on the first one.
#[cfg(feature = "compile")]
struct WasmIndex<'w, 'a> {
    wasm: &'w HashMap<ComponentRef, &'a [u8]>,
    by_world: OnceLock<HashMap<&'w PackageRef, HashMap<&'w str, World<'w, 'a>>>>,
}

#[cfg(feature = "compile")]
impl<'w, 'a> WasmIndex<'w, 'a> {
    fn new(wasm: &'w HashMap<ComponentRef, &'a [u8]>) -> Self {
        Self {
            wasm,
            by_world: OnceLock::new(),
        }
    }

    /// The keys of `id`'s world, with their bytes.
    fn world(&self, id: &ComponentRef) -> &[(&'w ComponentRef, &'a [u8])] {
        let wasm = self.wasm;
        self.by_world
            .get_or_init(|| {
                let mut by_world: HashMap<_, HashMap<_, World<'w, 'a>>> = HashMap::new();
                for (key, &bytes) in wasm {
                    by_world
                        .entry(&key.package)
                        .or_default()
                        .entry(key.world.as_str())
                        .or_default()
                        .push((key, bytes));
                }
                by_world
            })
            .get(&id.package)
            .and_then(|worlds| worlds.get(id.world.as_str()))
            .map_or(&[], Vec::as_slice)
    }

    /// The bytes keyed by exactly `id`.
    fn get(&self, id: &ComponentRef) -> Option<&'a [u8]> {
        self.wasm.get(id).copied()
    }

    /// The bytes for a component: keyed by its full ref, or by a ref
    /// without a content hash that matches it (same package, version
    /// included, and world).
    fn component(&self, id: &ComponentRef) -> Option<&'a [u8]> {
        self.get(id).or_else(|| {
            self.world(id)
                .iter()
                .find(|(key, _)| key.content_hash.is_none())
                .map(|(_, bytes)| *bytes)
        })
    }

    /// The bytes of a link's provider. A provider has no contract for a
    /// hash to name, so a pinned ref takes only its own key. An unpinned
    /// one takes its own key, else the one pinned key of its world:
    /// several are [`LoadError::AmbiguousProvider`], none
    /// [`LoadError::MissingWasm`].
    fn provider(&self, provider: &ComponentRef) -> Result<&'a [u8], LoadError> {
        let missing = || LoadError::MissingWasm {
            component: Box::new(provider.clone()),
        };
        if let Some(bytes) = self.get(provider) {
            return Ok(bytes);
        }
        if provider.content_hash.is_some() {
            return Err(missing());
        }
        // Its own key is not there, so every key of its world is pinned.
        match self.world(provider) {
            [] => Err(missing()),
            [(_, bytes)] => Ok(bytes),
            pinned => {
                let mut candidates: Vec<ComponentRef> =
                    pinned.iter().map(|(key, _)| (*key).clone()).collect();
                candidates.sort();
                Err(LoadError::AmbiguousProvider {
                    provider: Box::new(provider.clone()),
                    candidates,
                })
            }
        }
    }
}

/// Checks that `bytes` implement `contract`
/// ([`witgraph_wit::verify::verify_component`]), and returns the bytes' own
/// view of their world.
#[cfg(feature = "compile")]
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
