//! Loading: checking component bytes against their contracts, composing them
//! with a generated hub, and compiling the result.

#[cfg(feature = "precompiled")]
mod precompiled;

#[cfg(feature = "compile")]
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use wasmtime::Engine;
use wasmtime::component::Component;
#[cfg(feature = "compile")]
use witgraph_ir::ComponentContract;
use witgraph_ir::{Capability, CompiledGraph, IndexVec, NodeId, PortKind, PortTable};
#[cfg(feature = "compile")]
use witgraph_ir::{ComponentName, ContractId, LinkKey};
#[cfg(feature = "compile")]
use witgraph_wit::Artifact;
#[cfg(feature = "compile")]
use witgraph_wit::compose::{link_node, linked_providers};
#[cfg(feature = "compile")]
use witgraph_wit::hub::{self, HubMember};
#[cfg(feature = "compile")]
use witgraph_wit::verify::Imports;

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
/// Preparing is the slow part of a load: it checks the bytes target the worlds
/// the graph runs of them, generates the hub, composes it with the nodes, and compiles
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
    /// By node, the core modules of its component, to tell which node stops on a line.
    #[cfg(feature = "debug")]
    node_modules: Vec<(NodeId, Vec<u64>)>,
}

/// What a [`PreparedGraph`] is besides its compiled code.
#[derive(Clone)]
struct Parts {
    /// What tells the graph it was prepared for from any other.
    graph: String,
    /// What each node imports (with links composed in, what is left of the
    /// node's imports plus the providers'), as its WIT declares it: what the host
    /// is asked for. A component imports only what its code uses, which can be
    /// fewer than its world declares.
    capabilities: Vec<(NodeId, Vec<Capability>)>,
    /// How many components an instance holds: the hub, each node, and each
    /// provider composed in.
    components: usize,
    /// Whether its hub taps the streams between nodes, for
    /// [`RuntimeConfig::stream_previews`].
    stream_taps: bool,
    /// A hash of the bytes and links it was prepared from, when they were
    /// given: what tells a saved graph from one rebuilt with the same contracts.
    #[cfg(feature = "precompiled")]
    source: Option<String>,
}

/// What tells `compiled` from any other graph: its nodes, what they run, and how
/// they are wired. Anything here changes the composite. A contract's hash leaves
/// out its name and version, so components with the same ports are told apart by
/// those.
fn fingerprint(compiled: &CompiledGraph) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for node in compiled.topological_order() {
        let (component, hash) = compiled
            .contract_for(node)
            .map(|c| (c.id.to_string(), witgraph_wit::hash::content_hash(c)))
            .unwrap_or_default();
        let _ = writeln!(text, "node {node} {component} {hash}");
        for (import, provider, export) in compiled.link_keys(node) {
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
    /// The WIT of the node's component, and the world it runs.
    wit: &'a witgraph_wit::load::WitSource,
    world: &'a witgraph_ir::WorldRef,
    capabilities: Vec<Capability>,
    components: usize,
    /// The node's bytes, or the composed component's.
    bytes: Cow<'a, [u8]>,
}

#[cfg(feature = "compile")]
impl PreparedGraph {
    /// Checks `components` against `compiled`'s contracts, composes them, and
    /// compiles the result on `engine`.
    ///
    /// `components` maps each component the graph's table names (a node's, or a
    /// link's provider) to its bytes and the WIT its node manifest names. The
    /// bytes must target the world each node runs of them (wit-component's
    /// `targets`, [`LoadError::ContractMismatch`]); nothing else is read from
    /// them but the names they import. Bytes that are not a valid component, or
    /// that fail to compile on `engine`, are a [`LoadError::BadComponent`]; a
    /// graph that cannot be composed is a [`LoadError::Composition`]; an engine
    /// that cannot run a graph is an `InvalidConfig` ([`LoadError::Runtime`]).
    /// What the bytes import is what the host is asked for when the graph loads
    /// ([`Host::check`], [`Host::link`]).
    pub fn new(
        engine: &Engine,
        compiled: &CompiledGraph,
        components: &HashMap<ComponentName, Artifact<'_>>,
    ) -> Result<Self, LoadError> {
        Self::checked(engine, compiled, components, false, |_, _| Ok(()))
    }

    /// As [`new`](Self::new), with the streams between nodes tapped, so that a graph loaded
    /// from it with [`RuntimeConfig::stream_previews`] shows what they carry. Each of their
    /// items is copied through the hub then, and once more as text.
    pub fn with_stream_taps(
        engine: &Engine,
        compiled: &CompiledGraph,
        components: &HashMap<ComponentName, Artifact<'_>>,
    ) -> Result<Self, LoadError> {
        Self::checked(engine, compiled, components, true, |_, _| Ok(()))
    }

    /// As [`new`](Self::new), asking `check` about each node's imports before
    /// anything is composed or compiled.
    fn checked<'a>(
        engine: &Engine,
        compiled: &'a CompiledGraph,
        components: &'a HashMap<ComponentName, Artifact<'a>>,
        stream_taps: bool,
        check: impl Fn(&NodeId, &[Capability]) -> Result<(), LoadError>,
    ) -> Result<Self, LoadError> {
        validate_engine(engine)?;
        let artifact = |name: &ComponentName| {
            components.get(name).ok_or_else(|| LoadError::MissingWasm {
                component: name.clone(),
            })
        };
        // One component per contract and set of links: nodes of one component
        // with other links are other compositions. Each contract's bytes are
        // verified once, whatever their links.
        let mut checked: Vec<CheckedNode<'_>> = Vec::new();
        let mut index_of: HashMap<(&ContractId, Vec<LinkKey>), usize> = HashMap::new();
        let mut verified: HashMap<&ContractId, (Artifact<'a>, Imports, String)> = HashMap::new();
        let mut of_node = Vec::with_capacity(compiled.graph().nodes.len());
        for node in &compiled.graph().nodes {
            let contract = contract_of(compiled, &node.id)?;
            let key = (&contract.id, compiled.link_keys(&node.id));
            if let Some(&at) = index_of.get(&key) {
                of_node.push((&node.id, at));
                continue;
            }
            let (found, imports, export) = match verified.entry(&contract.id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let found = *artifact(&contract.id.component)?;
                    let (imports, export) = verify(contract, found)?;
                    entry.insert((found, imports, export))
                }
            };
            let links = linked_providers(imports, &key.1, |provider| {
                artifact(provider).map(|a| (a.bytes, a.capabilities))
            })?;
            let linked =
                link_node(contract, found.bytes, export, imports, &links).map_err(|e| {
                    LoadError::BadLink {
                        component: Box::new(contract.id.clone()),
                        import: e.import,
                        message: e.message,
                    }
                })?;
            checked.push(CheckedNode {
                export: linked.export,
                wit: found.wit,
                world: &contract.id.world,
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
                wit: checked[at].wit,
                world: checked[at].world,
                mocked: false,
            })
            .collect();
        let composed = hub::compose_graph(compiled, &members, hub::HubOptions { stream_taps })
            .map_err(|e| LoadError::Composition {
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
                stream_taps,
                #[cfg(feature = "precompiled")]
                source: Some(precompiled::source_hash(components)),
            },
            component,
            #[cfg(feature = "debug")]
            node_modules: crate::lines::node_modules(composed.instantiated.iter().filter_map(
                |node| {
                    let (_, at) = of_node.iter().find(|(id, _)| *id == node)?;
                    Some((node, checked[*at].bytes.as_ref()))
                },
            )),
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
            let Some(handle) = port.handle_type() else {
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
        components: &HashMap<ComponentName, Artifact<'_>>,
        config: RuntimeConfig,
        mode: M,
    ) -> Result<Self, LoadError> {
        Self::load_with_host(compiled, components, config, mode, NoCapabilities).await
    }
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Loads a compiled graph with its components, with `host` providing
    /// capability imports and the Store's data.
    ///
    /// The graph is prepared ([`PreparedGraph::new`], which says what
    /// `components` holds) on [`RuntimeConfig::engine`] (or a new engine), then loaded as
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
        components: &HashMap<ComponentName, Artifact<'_>>,
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
        let prepared = PreparedGraph::checked(
            &engine,
            &compiled,
            components,
            config.stream_previews,
            |node, capabilities| check_host(&host, node, capabilities),
        )?;
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
        if config.stream_previews && !prepared.parts.stream_taps {
            return Err(LoadError::Runtime(RuntimeError::InvalidConfig {
                message: "stream previews need a graph prepared with its streams tapped \
                          (`PreparedGraph::with_stream_taps`)"
                    .into(),
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
        let mut capabilities: Vec<Capability> = Vec::new();
        Capability::add_unique(
            &mut capabilities,
            prepared.parts.capabilities.iter().flat_map(|(_, c)| c),
        );
        let previews = config
            .stream_previews
            .then(|| crate::previews::StreamPreviews::new(&compiled, &ports));
        let linker = engine::graph_linker(&engine, &ports, &capabilities, previews.as_ref(), &host)
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
        #[cfg(feature = "debug")]
        let lines = if let Some(own) = &config.debug_lines {
            if !crate::lines::RuntimeLines::engine_can(&engine) {
                return Err(RuntimeError::InvalidConfig {
                    message: "`debug_lines` needs an engine made by `RuntimeConfig::new_engine` \
                              with it set"
                        .to_string(),
                }
                .into());
            }
            Some(crate::lines::RuntimeLines::new(
                engine.clone(),
                prepared.node_modules.clone(),
                own.clone(),
            ))
        } else {
            None
        };
        #[cfg(not(feature = "debug"))]
        let lines = crate::plane::NoLines;
        let mut first: Built<H::Data> = build(&engine, &pre, &exports, data, settings, &lines)
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
            lines,
            holds: crate::holds::RuntimeHolds::default(),
            previews,
            max_memory,
            components,
            shared_memory,
        };
        let core = SchedulerCore::new(
            compiled,
            mode.clone(),
            config.max_steps_per_tick,
            |_, ports| HubPlane::new(blueprint, ports.clone(), types.clone(), first),
        )?;
        Ok(Self {
            mode,
            core,
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
    // Only an engine made to debug gets a deadline: that is how it reaches a running guest.
    #[cfg(feature = "debug")]
    let debugging = crate::lines::RuntimeLines::engine_can(engine);
    #[cfg(not(feature = "debug"))]
    let debugging = false;
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
            !engine.get_epoch_interruption() || debugging,
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

/// Checks that `found` targets the world `contract` is of
/// ([`witgraph_wit::verify::verify_component`]), and returns what it imports,
/// and the name it exports its `node` interface under, as its WIT says.
#[cfg(feature = "compile")]
fn verify(
    contract: &ComponentContract,
    found: Artifact<'_>,
) -> Result<(Imports, String), LoadError> {
    use witgraph_wit::verify::VerifyError;
    let component = || Box::new(contract.id.clone());
    let imports =
        match witgraph_wit::verify::verify_component(found.wit, &contract.id.world, found.bytes) {
            Ok(imports) => imports,
            Err(VerifyError::Bad(message)) => {
                return Err(LoadError::BadComponent {
                    component: component(),
                    message,
                });
            }
            Err(e) => {
                return Err(LoadError::ContractMismatch {
                    component: component(),
                    message: e.to_string(),
                });
            }
        };
    let lowered = witgraph_wit::lower_world(found.wit, &contract.id).map_err(|e| {
        LoadError::BadComponent {
            component: component(),
            message: e.to_string(),
        }
    })?;
    Ok((imports, lowered.export))
}
