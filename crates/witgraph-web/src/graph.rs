//! `WebGraph`: a graph loaded on a JavaScript host.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{Either, Shared as SharedFuture};
use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use sha2::{Digest, Sha256};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};
use witgraph_ir::{ComponentContract, ComponentRef, Graph, NodeId, PortKind, PortName, PortRef};
use witgraph_sched::{
    NodeFault, RunShape, RuntimeError, Scheduler, Snapshot, TickResult, TraceEvent,
};

use crate::executor::{Callbacks, Js, OutputPort, Shared};
use crate::link::{self, Composed};
use crate::live::{Live, LiveMode, LiveState};
use crate::value::{Data, WebValue, from_js, set};
// The built-in interface, unversioned: as jco names the import.
use witgraph_wit::lower::HOST_INTERFACE;

type Sched = Scheduler<LiveMode, Js>;

/// The most trace events a graph keeps between two `takeTrace` calls; older
/// ones are dropped.
const TRACE_LIMIT: usize = 65_536;

/// The most contracts kept for later loads ([`CONTRACTS`]).
const CACHED: usize = 32;

#[wasm_bindgen]
extern "C" {
    // `Promise.resolve`, with its exception caught (see `value.rs`).
    #[wasm_bindgen(catch, js_namespace = Promise, js_name = resolve)]
    fn resolve(value: &JsValue) -> Result<Promise, JsValue>;
}

/// What a component's contract is made from: the SHA-256 of its bytes and
/// of its WIT source, and its id as given.
#[derive(PartialEq, Eq)]
struct ContractKey {
    bytes: [u8; 32],
    wit: Option<[u8; 32]>,
    id: String,
}

thread_local! {
    /// The contracts the latest loads made, latest last: an editor that
    /// reloads its graph on every edit lowers each component once.
    static CONTRACTS: RefCell<Vec<(ContractKey, Lowered)>> =
        const { RefCell::new(Vec::new()) };
}

pub(crate) fn error(message: impl std::fmt::Display) -> JsError {
    JsError::new(&message.to_string())
}

fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

/// The error for a call made while a tick holds the scheduler.
fn busy() -> JsError {
    error("a tick is in progress")
}

/// An optional function property of `object`.
fn function(object: &JsValue, name: &str) -> Result<Option<Function>, JsError> {
    let value = Reflect::get(object, &text(name)).unwrap_or(JsValue::UNDEFINED);
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .dyn_into::<Function>()
        .map(Some)
        .map_err(|_| error(format!("`{name}` is not a function")))
}

/// A component as the editor catalog describes one
/// ([`ComponentMeta`](witgraph_wit::metadata::ComponentMeta)), as a
/// JavaScript object.
fn describe(meta: &witgraph_wit::metadata::ComponentMeta) -> Result<JsValue, JsError> {
    json(meta)
}

/// `value` as a JavaScript object, through its JSON form.
fn json(value: &impl serde::Serialize) -> Result<JsValue, JsError> {
    let json = serde_json::to_string(value).map_err(error)?;
    js_sys::JSON::parse(&json).map_err(|_| error("a value did not convert from JSON"))
}

/// A component's contract, and what was wrong with it but did not stop it
/// lowering (see [`witgraph_wit::lower::Lowered::warnings`]).
#[derive(Clone)]
struct Lowered {
    contract: ComponentContract,
    warnings: Vec<String>,
}

/// The contract of component `id` with encoded `bytes` (whose SHA-256 is
/// `digest`) and, if given, the WIT source it was built from: as a load
/// made it before, else lowered now.
fn contract(
    id: &str,
    bytes: &[u8],
    digest: [u8; 32],
    wit: Option<&str>,
) -> Result<Lowered, JsError> {
    let key = ContractKey {
        bytes: digest,
        wit: wit.map(|wit| Sha256::digest(wit).into()),
        id: id.to_owned(),
    };
    let cached = CONTRACTS.with_borrow_mut(|contracts| {
        let index = contracts.iter().position(|(k, _)| *k == key)?;
        let entry = contracts.remove(index);
        let contract = entry.1.clone();
        contracts.push(entry);
        Some(contract)
    });
    if let Some(contract) = cached {
        return Ok(contract);
    }
    let contract = lower(id, bytes, wit)?;
    CONTRACTS.with_borrow_mut(|contracts| {
        contracts.push((key, contract.clone()));
        if contracts.len() > CACHED {
            contracts.remove(0);
        }
    });
    Ok(contract)
}

/// The contract of component `id`: from its WIT source, against which the
/// bytes are checked, or else lowered from the bytes. Either way its
/// outputs' viewers are the bytes' (WIT text declares none).
fn lower(id: &str, bytes: &[u8], wit: Option<&str>) -> Result<Lowered, JsError> {
    let id: ComponentRef = id.parse().map_err(error)?;
    match wit {
        Some(wit) => {
            let name = format!("{id}.wit");
            let mut contract = witgraph_wit::contract_from_str(&name, wit, &id)
                .map_err(|e| error(format!("the WIT of `{id}`: {e}")))?;
            let found = witgraph_wit::verify::verify_component(&contract, bytes)
                .map_err(|e| error(format!("component `{id}`: {e}")))?;
            for port in &mut contract.outputs {
                port.viewer = found
                    .contract
                    .outputs
                    .iter()
                    .find(|p| p.name == port.name)
                    .and_then(|p| p.viewer.clone());
            }
            Ok(Lowered {
                contract,
                warnings: found.warnings,
            })
        }
        None => {
            let lowered = witgraph_wit::lower_component(bytes, &id).map_err(error)?;
            let contract = lowered.contract;
            // Without WIT, the bytes' own hash is all there is: a pin
            // naming another is other bytes than these.
            if let (Some(pinned), Some(own)) = (&id.content_hash, &contract.id.content_hash)
                && pinned != own
            {
                return Err(error(format!(
                    "component `{id:#}`: the bytes hash to {own}; pass `wit` to check them \
                     against the source the pin names"
                )));
            }
            Ok(Lowered {
                contract,
                warnings: lowered.warnings,
            })
        }
    }
}

/// `bytes` as lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An interface id without its version (`a:b/c@1.0.0` is `a:b/c`), as jco
/// names an import.
fn unversioned(name: &str) -> &str {
    name.split_once('@').map_or(name, |(id, _)| id)
}

/// Lowers the WIT embedded in an encoded component and describes it as the
/// editor catalog does (`{ id, package, world, content_hash, docs, inputs,
/// outputs, capabilities, types }`, a port being `{ name, kind,
/// type_display, handle?, optional, docs?, viewer? }`). `id` names the component
/// (`namespace:name/world@version`); the result's `id` carries the content
/// hash computed from the bytes.
#[wasm_bindgen(js_name = describeComponent)]
pub fn describe_component(id: &str, bytes: &[u8]) -> Result<JsValue, JsError> {
    let id: ComponentRef = id.parse().map_err(error)?;
    let lowered = witgraph_wit::lower_component(bytes, &id).map_err(error)?;
    let catalog = witgraph_wit::metadata::generate_catalog(std::slice::from_ref(&lowered));
    let meta = catalog
        .components
        .first()
        .ok_or_else(|| error("the component did not describe"))?;
    describe(meta)
}

/// The name jco gives a WIT record field or flag in JavaScript
/// (`HTTP-status` is `httpStatus`): the one rule the executor and the
/// loader share.
#[wasm_bindgen(js_name = jsName)]
pub fn js_name(name: &str) -> String {
    crate::value::camel(name)
}

/// A compiled graph running on this JavaScript host.
#[wasm_bindgen]
pub struct WebGraph {
    /// The scheduler; `None` while a tick has it.
    sched: Rc<RefCell<Option<Sched>>>,
    /// What the read methods report while a tick has `sched`.
    live: Arc<Live>,
    shared: Rc<Shared>,
    /// Per node, the index of the `components` entry it runs.
    components: HashMap<NodeId, u32>,
    /// Per `components` entry, the SHA-256 of its bytes, in hex.
    digests: Vec<String>,
    /// The nodes' components with their links composed in.
    composed: Vec<Composed>,
    /// Per node with links that apply, the index in `composed` of what it
    /// runs.
    composed_of: HashMap<NodeId, usize>,
    /// What was wrong with the components but did not stop the load.
    warnings: Vec<String>,
    /// Dropped with the graph, which completes `freed`.
    _alive: oneshot::Sender<()>,
    /// Completes once the graph is freed: a tick still running then ends.
    freed: SharedFuture<oneshot::Receiver<()>>,
}

#[wasm_bindgen]
impl WebGraph {
    /// Compiles and loads a graph.
    ///
    /// - `graph` is the graph as JSON (`witgraph_ir::Graph`).
    /// - `components` is an array of `{ id, bytes, wit? }`: every
    ///   component the graph instantiates, by reference
    ///   (`namespace:name/world@version`) and encoded component bytes.
    ///   `wit` is the WIT source text the component was built from (one
    ///   document, dependencies nested in it): its world's contract is the
    ///   one a catalog lowered from that source has, the bytes are checked
    ///   against it as the wasmtime runtime checks them, and the graph
    ///   resolves against it, so a graph pinning the catalog's content
    ///   hashes loads. Without it the contract is lowered from the bytes,
    ///   whose content hash is the bytes' own (only the imports the code
    ///   uses count). Which entry each node runs is `component(node)`.
    ///   Contracts are kept for later loads, by the SHA-256 of the bytes
    ///   and of `wit`.
    /// - `callbacks` is `{ run, rebuild, close?, abandon? }`:
    ///   `run(node, inputs)` calls the node's `run` (as jco transpiles it)
    ///   and returns its outputs or a promise of them; `rebuild(node)`
    ///   gives the node fresh guest state (a new instance), throwing or
    ///   rejecting if it cannot (with an error marked `witgraphFatal` when
    ///   a start function called `fatal`); `close(value)` drops a stream
    ///   or future nothing reads; `abandon(nodes)` says the generation of
    ///   those nodes ended before their `rebuild` or `run` calls did (a
    ///   cancel, shutdown or restore, or a sibling's failed `run`), so
    ///   whatever those calls still do belongs to nothing.
    ///
    /// A graph that does not compile throws with every diagnostic.
    pub fn load(graph: &str, components: Array, callbacks: JsValue) -> Result<WebGraph, JsError> {
        let graph: Graph = serde_json::from_str(graph).map_err(error)?;
        let callbacks = Callbacks {
            run: function(&callbacks, "run")?
                .ok_or_else(|| error("`callbacks.run` is required"))?,
            rebuild: function(&callbacks, "rebuild")?
                .ok_or_else(|| error("`callbacks.rebuild` is required"))?,
            close: function(&callbacks, "close")?,
            abandon: function(&callbacks, "abandon")?,
        };
        let mut contracts = Vec::new();
        let mut warnings = Vec::new();
        // Per contract, the `components` entry it came from.
        let mut contract_entry = Vec::new();
        let mut entries: Vec<(ComponentRef, Vec<u8>)> = Vec::new();
        let mut digests = Vec::new();
        for (index, entry) in components.iter().enumerate() {
            let id = Reflect::get(&entry, &text("id"))
                .ok()
                .and_then(|id| id.as_string())
                .ok_or_else(|| error("a component needs an `id`"))?;
            // `isView` sees through no proxy: what passes copies out
            // without calling back into JavaScript.
            let bytes = Reflect::get(&entry, &text("bytes"))
                .ok()
                .filter(ArrayBuffer::is_view)
                .and_then(|bytes| bytes.dyn_into::<Uint8Array>().ok())
                .ok_or_else(|| error(format!("component `{id}` needs `bytes`")))?
                .to_vec();
            let wit = Reflect::get(&entry, &text("wit"))
                .ok()
                .and_then(|wit| wit.as_string());
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let component: ComponentRef = id.parse().map_err(error)?;
            // A provider is not a node: it has no `node` export, so no
            // contract. An entry that only links name is composed in, not
            // lowered as a node.
            let provider_only = graph.links.iter().any(|l| l.provider.matches(&component))
                && !graph.components.iter().any(|c| c.matches(&component));
            if !provider_only {
                let lowered = contract(&id, &bytes, digest, wit.as_deref())?;
                warnings.extend(
                    lowered
                        .warnings
                        .into_iter()
                        .map(|w| format!("component `{id}`: {w}")),
                );
                contracts.push(lowered.contract);
                contract_entry.push(index);
            }
            digests.push(hex(&digest));
            entries.push((component, bytes));
        }
        let compiled = graph.compile(&contracts).map_err(|failure| {
            let diagnostics: Vec<String> = failure
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect();
            error(format!(
                "the graph does not compile:\n{}",
                diagnostics.join("\n")
            ))
        })?;
        // Among entries with one contract, compilation binds the first.
        let mut first_of: HashMap<&ComponentRef, usize> = HashMap::new();
        for (index, contract) in contracts.iter().enumerate() {
            first_of.entry(&contract.id).or_insert(index);
        }
        let mut node_components = HashMap::new();
        for node in compiled.nodes() {
            let index = first_of
                .get(&node.contract.id)
                .and_then(|&index| u32::try_from(contract_entry[index]).ok())
                .ok_or_else(|| error(format!("node `{}` has no component", node.node.id)))?;
            node_components.insert(node.node.id.clone(), index);
        }
        let linked = link::compose_links(&compiled, &entries, &node_components)?;

        let mut shared = Shared {
            callbacks,
            input_types: HashMap::new(),
            output_types: HashMap::new(),
            outputs: HashMap::new(),
            latest: RefCell::new(HashMap::new()),
            touched: RefCell::new(HashSet::new()),
        };
        let mut islands = Vec::new();
        for members in compiled.islands() {
            let mut shapes = Vec::with_capacity(members.len());
            for node in members {
                let contract = compiled
                    .contract_for(node)
                    .ok_or_else(|| error(format!("unknown node `{node}`")))?;
                for port in &contract.inputs {
                    if let (PortKind::Value, Some(ty)) = (port.kind, &port.ty) {
                        let port = PortRef::new(node.clone(), port.name.clone());
                        shared.input_types.insert(port, ty.clone());
                    }
                }
                let mut outputs = Vec::new();
                for port in &contract.outputs {
                    if let (PortKind::Value, Some(ty)) = (port.kind, &port.ty) {
                        let port = PortRef::new(node.clone(), port.name.clone());
                        shared.output_types.insert(port, ty.clone());
                    }
                    outputs.push(OutputPort {
                        name: port.name.to_string(),
                        kind: port.kind,
                        ty: port.ty.clone(),
                    });
                }
                shared.outputs.insert(node.clone(), outputs);
                shapes.push(RunShape {
                    inputs: (!contract.inputs.is_empty())
                        .then(|| contract.inputs.iter().map(|p| p.name.to_string()).collect()),
                    has_result: !contract.outputs.is_empty(),
                });
            }
            islands.push(((), shapes));
        }
        let shared = Rc::new(shared);
        let executor = Js {
            shared: shared.clone(),
        };
        let live = Live::new(TRACE_LIMIT);
        let mode = LiveMode(live.clone());
        let sched = Scheduler::new(
            compiled,
            mode,
            executor,
            islands,
            witgraph_sched::DEFAULT_MAX_STEPS_PER_TICK,
        )
        .map_err(error)?;
        refresh(&sched, &live, &shared);
        let (alive, freed) = oneshot::channel();
        Ok(Self {
            sched: Rc::new(RefCell::new(Some(sched))),
            live,
            shared,
            components: node_components,
            digests,
            composed: linked.composed,
            composed_of: linked.of_node,
            warnings,
            _alive: alive,
            freed: freed.shared(),
        })
    }

    /// Runs one iteration of the graph (see `Scheduler::tick`). Resolves to
    /// `{ result }`, one of `progress`, `idle`, `interrupted`,
    /// `step-limit`, or `aborted` with `node` and `message`. When `stop` (a
    /// promise) settles first, the tick ends early with `interrupted`, as
    /// it does when the graph is freed.
    ///
    /// One tick runs at a time: while one does (paused in a `run`
    /// callback, say), another rejects at once. `readOutput`,
    /// `readOutputWave`, `nodeState`, `takeTrace` and `component` report
    /// what the tick has done so far; the other methods throw.
    pub fn tick(&self, stop: JsValue) -> Promise {
        self.start_tick(&stop)
            .unwrap_or_else(|e| Promise::reject(&JsValue::from(e)))
    }

    fn start_tick(&self, stop: &JsValue) -> Result<Promise, JsError> {
        let stop = if stop.is_undefined() || stop.is_null() {
            None
        } else {
            let then = Reflect::get(stop, &text("then")).unwrap_or(JsValue::UNDEFINED);
            if !then.is_function() {
                return Err(error("`stop` is not a promise"));
            }
            let stop = resolve(stop).map_err(|_| error("`stop` is not a promise"))?;
            Some(JsFuture::from(stop))
        };
        let mut sched = self
            .sched
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take())
            .ok_or_else(busy)?;
        let slot = self.sched.clone();
        let (live, shared) = (self.live.clone(), self.shared.clone());
        let freed = self.freed.clone().map(|_| ());
        Ok(future_to_promise(async move {
            let stop = match stop {
                Some(stop) => Either::Left(futures::future::select(stop, freed).map(|_| ())),
                None => Either::Right(freed),
            };
            let result = sched.tick_until(stop).await;
            reconcile(&sched, &live, &shared);
            // Back for the other methods (dropped here if the graph was
            // freed meanwhile). Nothing borrows the slot across a turn of
            // the event loop, so it is free.
            slot.replace(Some(sched));
            let object = Object::new();
            let name = match &result {
                TickResult::Progress => "progress",
                TickResult::Idle => "idle",
                TickResult::Interrupted => "interrupted",
                TickResult::StepLimitReached => "step-limit",
                TickResult::Aborted { node, fault } => {
                    set(&object, "node", &text(node.as_str()));
                    set(&object, "message", &text(&fault.to_string()));
                    "aborted"
                }
            };
            set(&object, "result", &text(name));
            Ok(object.into())
        }))
    }

    /// The scheduler, unless a tick has it.
    fn idle(&self) -> Result<RefMut<'_, Sched>, JsError> {
        let slot = self.sched.try_borrow_mut().map_err(|_| busy())?;
        RefMut::filter_map(slot, Option::as_mut).map_err(|_| busy())
    }

    /// The scheduler to read from, unless a tick has it.
    fn readable(&self) -> Option<Ref<'_, Sched>> {
        Ref::filter_map(self.sched.try_borrow().ok()?, Option::as_ref).ok()
    }

    /// The `components` entry node `node` runs: `{ index, sha256, bytes? }`,
    /// its position in the array `load` was given and the SHA-256 of its
    /// bytes (hex). A node whose links apply runs those bytes with its
    /// providers composed in: `bytes` are the composed component and
    /// `sha256` theirs, and `index` is still the node's own entry.
    pub fn component(&self, node: &str) -> Result<JsValue, JsError> {
        let index = *self
            .components
            .get(&NodeId::from(node))
            .ok_or_else(|| error(format!("unknown node `{node}`")))?;
        let digest = self
            .digests
            .get(index as usize)
            .ok_or_else(|| error(format!("unknown node `{node}`")))?;
        let object = Object::new();
        set(&object, "index", &JsValue::from(index));
        match self.composed_of.get(&NodeId::from(node)) {
            Some(&at) => {
                let composed = &self.composed[at];
                set(&object, "sha256", &text(&composed.sha256));
                set(
                    &object,
                    "bytes",
                    &Uint8Array::from(composed.bytes.as_slice()),
                );
            }
            None => set(&object, "sha256", &text(digest)),
        }
        Ok(object.into())
    }

    /// What each of a node's imports is, by the name `jco transpile` lists
    /// it under (an interface id without its version, or a label): the
    /// capability of the node's contract it is, `{ interface, implements?,
    /// items? }` as a wasmtime host is asked for it (`interface` the full
    /// id, version included, or the label, with `implements` the interface
    /// the label stands for), or `null` for the built-in
    /// `witgraph:runtime/host`. An import that is neither throws.
    pub fn imports(&self, node: &str, names: Vec<String>) -> Result<Array, JsError> {
        let sched = self.idle()?;
        let contract = sched
            .compiled()
            .contract_for(&node.into())
            .ok_or_else(|| error(format!("unknown node `{node}`")))?;
        // A composed node imports what its providers do as well, and not
        // what they satisfy.
        let capabilities = match self.composed_of.get(&NodeId::from(node)) {
            Some(&at) => &self.composed[at].capabilities,
            None => &contract.capabilities,
        };
        names
            .iter()
            .map(|name| {
                let named = |by: fn(&str) -> &str| -> Vec<_> {
                    capabilities
                        .iter()
                        .filter(|c| by(c.link_name()) == name)
                        .collect()
                };
                let mut found = named(|n| n);
                if found.is_empty() {
                    found = named(unversioned);
                }
                match found.as_slice() {
                    [capability] => json(capability),
                    [] if unversioned(name) == HOST_INTERFACE => Ok(JsValue::NULL),
                    [] => Err(error(format!(
                        "node `{node}` imports `{name}`, which its contract does not list"
                    ))),
                    many => Err(error(format!(
                        "node `{node}` imports `{name}`, which several capabilities match: {}",
                        many.iter()
                            .map(|c| format!("`{}`", c.interface))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                }
            })
            .collect()
    }

    /// Writes a value, in jco's JavaScript representation, to a Value
    /// input port.
    pub fn inject(&self, node: &str, port: &str, value: JsValue) -> Result<(), JsError> {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        // Checked as `inject` checks it: a connected input is refused
        // before its value is looked at.
        let ty = self
            .idle()?
            .host_input_type(&node, &port)
            .cloned()
            .map_err(error)?;
        // Not under the scheduler: converting may call back into
        // JavaScript (a getter), and that into this graph.
        let value = from_js(&value, &ty).map_err(error)?;
        self.idle()?
            .inject(
                &node,
                &port,
                WebValue::Data(Rc::new(Data::from_value(value, ty))),
            )
            .map_err(error)
    }

    /// Writes a value, as WAVE text, to a Value input port.
    #[wasm_bindgen(js_name = injectWave)]
    pub fn inject_wave(&self, node: &str, port: &str, wave: &str) -> Result<(), JsError> {
        self.idle()?
            .inject_wave(&node.into(), &port.into(), wave)
            .map_err(error)
    }

    /// Clears an injected Value input.
    #[wasm_bindgen(js_name = clearInput)]
    pub fn clear_input(&self, node: &str, port: &str) -> Result<(), JsError> {
        self.idle()?
            .clear_input(&node.into(), &port.into())
            .map_err(error)
    }

    /// Makes the node's island owe a run on its latched inputs.
    pub fn rerun(&self, node: &str) -> Result<(), JsError> {
        self.idle()?.rerun(&node.into()).map_err(error)
    }

    /// Cancels the node's island.
    pub fn cancel(&self, node: &str) -> Result<(), JsError> {
        self.idle()?.cancel(&node.into()).map_err(error)
    }

    /// Stops every island.
    pub fn shutdown(&self) -> Result<(), JsError> {
        self.idle()?.shutdown();
        Ok(())
    }

    /// The latched value of a Value or handle output port: from the
    /// scheduler, or, while a tick holds it, from the tick so far.
    fn output(&self, node: &str, port: &str) -> Result<Option<WebValue>, JsError> {
        let port_ref = PortRef::new(node, port);
        // As the scheduler checks it, which a tick may be holding.
        let Some(outputs) = self.shared.outputs.get(&port_ref.node) else {
            return Err(error(format!("unknown node `{node}`")));
        };
        if !outputs
            .iter()
            .any(|p| p.name == port && p.kind.is_latched())
        {
            return Err(error(format!(
                "`{node}.{port}` is not a Value or handle output port"
            )));
        }
        Ok(match self.readable() {
            Some(sched) => sched
                .read_output(&port_ref.node, &port_ref.port)
                .map_err(error)?,
            None => self.shared.latest.borrow().get(&port_ref).cloned(),
        })
    }

    /// The latched value of a Value output port, in jco's JavaScript
    /// representation, or the object of a handle output port, as its maker
    /// returned it; `undefined` before the node has run. While a tick runs,
    /// the latest value a `run` of it returned. The value is the one the
    /// node's consumers are given, not a copy: it must not be changed. A
    /// handle's object is disposed once no latch holds it.
    #[wasm_bindgen(js_name = readOutput)]
    pub fn read_output(&self, node: &str, port: &str) -> Result<JsValue, JsError> {
        Ok(match self.output(node, port)? {
            Some(WebValue::Data(data)) => data.js().clone(),
            Some(WebValue::Resource(resource)) => resource.js().clone(),
            _ => JsValue::UNDEFINED,
        })
    }

    /// The latched value of a Value output port as WAVE text; `undefined`
    /// before the node has run. While a tick runs, as `readOutput`. Throws,
    /// naming the port, for a value too large to convert (see
    /// `readOutput` for the value itself), and for a handle, which has no
    /// text.
    #[wasm_bindgen(js_name = readOutputWave)]
    pub fn read_output_wave(&self, node: &str, port: &str) -> Result<Option<String>, JsError> {
        match self.output(node, port)? {
            Some(WebValue::Data(data)) => data
                .wave()
                .map(Some)
                .map_err(|e| error(format!("`{node}.{port}`: {e}"))),
            Some(WebValue::Resource(_)) => Err(error(format!(
                "`{node}.{port}` is a handle: it has no WAVE text"
            ))),
            _ => Ok(None),
        }
    }

    /// A node's lifecycle state: `{ phase, fault?, culprit? }`. While a
    /// tick runs, its phase and fault as of the last change; a fault the
    /// tick causes names its culprit once the tick ends.
    #[wasm_bindgen(js_name = nodeState)]
    pub fn node_state(&self, node: &str) -> Result<JsValue, JsError> {
        let state = match self.readable() {
            Some(sched) => {
                let state = sched.node_state(&node.into()).map_err(error)?;
                live_state(&state)
            }
            None => self
                .live
                .state(&node.into())
                .ok_or_else(|| error(format!("unknown node `{node}`")))?,
        };
        let object = Object::new();
        set(&object, "phase", &phase(state.phase));
        if let Some(fault) = &state.fault {
            set(&object, "fault", &fault_object(fault));
        }
        if let Some(culprit) = &state.culprit {
            set(&object, "culprit", &text(culprit.as_str()));
        }
        Ok(object.into())
    }

    /// Takes the faults reported since the last call: an array of
    /// `{ members, culprit?, fault }`.
    #[wasm_bindgen(js_name = takeFaults)]
    pub fn take_faults(&self) -> Result<Array, JsError> {
        let faults = self.idle()?.take_faults();
        Ok(faults
            .iter()
            .map(|report| {
                let object = Object::new();
                let members: Array = report.members.iter().map(|m| text(m.as_str())).collect();
                set(&object, "members", &members);
                if let Some(culprit) = &report.culprit {
                    set(&object, "culprit", &text(culprit.as_str()));
                }
                set(&object, "fault", &fault_object(&report.fault));
                JsValue::from(object)
            })
            .collect())
    }

    /// Takes the trace recorded since the last call: an array of events,
    /// each `{ event, ... }`. Only the latest 65 536 events are kept. It
    /// works while a tick runs too.
    #[wasm_bindgen(js_name = takeTrace)]
    pub fn take_trace(&self) -> Array {
        self.live.take_trace().iter().map(trace_event).collect()
    }

    /// The graph's host-visible state, as JSON (`Snapshot`).
    pub fn snapshot(&self) -> Result<String, JsError> {
        let snapshot = self.idle()?.snapshot().map_err(error)?;
        serde_json::to_string(&snapshot).map_err(error)
    }

    /// Replaces the graph's host-visible state with a snapshot, given as
    /// JSON. Every node gets fresh guest state (`rebuild`) when it next
    /// runs.
    pub fn restore(&self, snapshot: &str) -> Result<(), JsError> {
        let snapshot: Snapshot = serde_json::from_str(snapshot).map_err(error)?;
        let mut sched = self.idle()?;
        sched
            .restore(&snapshot)
            .map_err(|e: RuntimeError| error(e))?;
        refresh(&sched, &self.live, &self.shared);
        Ok(())
    }

    /// A node's contract, as `describeComponent` describes one (without
    /// named types).
    pub fn contract(&self, node: &str) -> Result<JsValue, JsError> {
        let sched = self.idle()?;
        let contract = sched
            .compiled()
            .contract_for(&node.into())
            .ok_or_else(|| error(format!("unknown node `{node}`")))?;
        describe(&witgraph_wit::metadata::ComponentMeta::of(contract))
    }

    /// What was wrong with the components but did not stop the load, one
    /// message each (a viewer hint naming an output the component does not
    /// have, say): strings.
    pub fn warnings(&self) -> Array {
        self.warnings.iter().map(|w| JsValue::from_str(w)).collect()
    }

    /// The islands of the compiled graph: arrays of node ids, in island
    /// order.
    pub fn islands(&self) -> Result<Array, JsError> {
        let sched = self.idle()?;
        Ok(sched
            .compiled()
            .islands()
            .iter()
            .map(|members| {
                let members: Array = members.iter().map(|m| text(m.as_str())).collect();
                JsValue::from(members)
            })
            .collect())
    }
}

/// A node's state, as the live view keeps it.
fn live_state(state: &witgraph_sched::NodeState) -> LiveState {
    LiveState {
        phase: state.phase(),
        fault: state.fault_cause().cloned(),
        culprit: state.culprit().cloned(),
    }
}

/// Copies what the read methods report into the live view, whole: every
/// node's state and every latched Value output. At load and on a restore.
fn refresh(sched: &Sched, live: &Live, shared: &Shared) {
    for node in &sched.compiled().graph().nodes {
        if let Ok(state) = sched.node_state(&node.id) {
            live.set_state(node.id.clone(), live_state(&state));
        }
    }
    live.take_touched();
    shared.touched.borrow_mut().clear();
    let latest = shared
        .output_types
        .keys()
        .filter_map(|port| {
            Some((
                port.clone(),
                sched.read_output(&port.node, &port.port).ok()??,
            ))
        })
        .collect();
    *shared.latest.borrow_mut() = latest;
}

/// Makes exact, from the scheduler, what changed in the live view since it
/// last was: when a tick ends. Only what was touched is copied.
fn reconcile(sched: &Sched, live: &Live, shared: &Shared) {
    for node in live.take_touched() {
        if let Ok(state) = sched.node_state(&node) {
            live.set_state(node, live_state(&state));
        }
    }
    let touched: Vec<PortRef> = shared.touched.borrow_mut().drain().collect();
    let mut latest = shared.latest.borrow_mut();
    for port in touched {
        match sched.read_output(&port.node, &port.port) {
            Ok(Some(value)) => latest.insert(port, value),
            _ => latest.remove(&port),
        };
    }
}

fn phase(phase: witgraph_sched::NodePhase) -> JsValue {
    text(match phase {
        witgraph_sched::NodePhase::Pending => "pending",
        witgraph_sched::NodePhase::Running => "running",
        witgraph_sched::NodePhase::Idle => "idle",
        witgraph_sched::NodePhase::Faulted => "faulted",
        witgraph_sched::NodePhase::Cancelled => "cancelled",
    })
}

/// A fault as `{ kind, message }`.
fn fault_object(fault: &NodeFault) -> JsValue {
    let kind = match fault {
        NodeFault::WasmTrap { .. } => "trap",
        NodeFault::MemoryLimit { .. } => "memory-limit",
        NodeFault::HostcallFuelExhausted => "hostcall-fuel",
        NodeFault::FuelExhausted => "fuel",
        NodeFault::Fatal { .. } => "fatal",
        NodeFault::Restart { .. } => "restart",
    };
    let object = Object::new();
    set(&object, "kind", &text(kind));
    set(&object, "message", &text(&fault.to_string()));
    object.into()
}

fn trace_event(event: &TraceEvent) -> JsValue {
    let object = Object::new();
    let named = |name: &str| set(&object, "event", &text(name));
    let node = |node: &NodeId| set(&object, "node", &text(node.as_str()));
    let generation = |island: usize, generation: u64| {
        set(&object, "island", &JsValue::from(island as u32));
        set(&object, "generation", &JsValue::from(generation as f64));
    };
    match event {
        TraceEvent::GenerationStarted {
            island,
            generation: number,
        } => {
            named("generation-started");
            generation(*island, *number);
        }
        TraceEvent::RunStarted { node: id } => {
            named("run-started");
            node(id);
        }
        TraceEvent::RunReturned { node: id } => {
            named("run-returned");
            node(id);
        }
        TraceEvent::GenerationFinished {
            island,
            generation: number,
        } => {
            named("generation-finished");
            generation(*island, *number);
        }
        TraceEvent::GenerationStopped {
            island,
            generation: number,
        } => {
            named("generation-stopped");
            generation(*island, *number);
        }
        TraceEvent::PhaseTransition { node: id, from, to } => {
            named("phase-transition");
            node(id);
            set(&object, "from", &phase(*from));
            set(&object, "to", &phase(*to));
        }
        TraceEvent::Fault { node: id, fault } => {
            named("fault");
            node(id);
            set(&object, "fault", &fault_object(fault));
        }
        TraceEvent::Cancelled { node: id } => {
            named("cancelled");
            node(id);
        }
        TraceEvent::Restarted { node: id } => {
            named("restarted");
            node(id);
        }
        TraceEvent::StreamItems {
            node: id,
            port,
            count,
        } => {
            named("stream-items");
            node(id);
            set(&object, "port", &text(port.as_str()));
            set(&object, "count", &JsValue::from(*count as u32));
        }
    }
    object.into()
}
