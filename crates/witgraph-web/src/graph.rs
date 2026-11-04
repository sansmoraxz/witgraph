//! `WebGraph`: a graph loaded on a JavaScript host.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{Either, Shared as SharedFuture};
use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use sha2::{Digest, Sha256};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};
use witgraph_ir::wasm_wave::value::Type;
use witgraph_ir::{
    Capability, CompiledGraph, ComponentContract, ComponentRef, Graph, Idx, IndexVec, InputIdx,
    NodeId, OutputIdx, PortDirection, PortKind, PortName, PortTable,
};
use witgraph_sched::{
    DataPlane, NodeFault, PortVersions, RuntimeError, SchedulerCore, Snapshot, TickResult,
    TraceEvent, latched_output, not_a_value_output,
};
use witgraph_traits::{
    FaultReport, NodeState, OutputReader, PortVersion, RunningGraph, SnapshotError,
};

use crate::link::{self, Composed};
use crate::live::{Live, LiveMode, LiveState};
use crate::plane::{Callbacks, WebHub, WebPlane, stream_inputs};
use crate::value::{Data, check_type, from_js, parse_wave, set, set_field, with_key};
use witgraph_wit::hub::{self, HubMember};

/// Reads a graph's latched outputs, as jco represents them, also while it
/// ticks.
#[derive(Clone)]
pub(crate) struct WebReader {
    hub: Rc<WebHub>,
    versions: PortVersions,
}

impl WebReader {
    /// Checks that `node` exists and `port` is an output of it of a latched kind.
    fn latched(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(OutputIdx, PortKind), RuntimeError> {
        latched_output(&self.hub.ports, node, port)
    }

    fn value(&self, node: &NodeId, port: &PortName) -> Result<Option<Rc<Data>>, RuntimeError> {
        let (output, kind) = self.latched(node, port)?;
        if kind != PortKind::Value {
            return Err(not_a_value_output(node, port));
        }
        if !self.hub.live.get() {
            return Ok(None);
        }
        self.hub
            .host_read(self.hub.get_output(output))
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })
    }

    /// The latched value of a Value input port.
    fn input(&self, node: &NodeId, port: &PortName) -> Result<Option<JsValue>, RuntimeError> {
        let input =
            self.hub
                .ports
                .input(node, port)
                .ok_or_else(|| RuntimeError::NotAValuePort {
                    node: node.clone(),
                    port: port.clone(),
                    direction: PortDirection::Input,
                })?;
        if !self.hub.live.get() || self.hub.input_types[input].is_none() {
            return Ok(None);
        }
        let data = self
            .hub
            .host_read(self.hub.get_input(input))
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })?;
        Ok(data.map(|data| data.js().clone()))
    }
}

impl OutputReader for WebReader {
    type Value = JsValue;
    type Handle = JsValue;

    async fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<JsValue>, RuntimeError> {
        Ok(self.value(node, port)?.map(|data| data.js().clone()))
    }

    async fn read_output_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        self.value(node, port)?
            .map(|data| data.wave())
            .transpose()
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })
    }

    async fn read_handle(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<JsValue>, RuntimeError> {
        let (output, kind) = self.latched(node, port)?;
        if kind != PortKind::Handle {
            return Err(not_a_value_output(node, port));
        }
        if !self.hub.live.get() {
            return Ok(None);
        }
        self.hub
            .host_read(self.hub.read_handle(output))
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.latched(node, port)
            .map(|(output, _)| self.versions.get(output))
    }
}

/// The scheduler on this host, as [`RunningGraph`] has it.
pub(crate) struct WebRuntime {
    core: SchedulerCore<WebPlane>,
    reader: WebReader,
}

/// A failure of a port call, of `node.port`.
fn port_error(node: &NodeId, port: &PortName, message: String) -> RuntimeError {
    RuntimeError::ValueType {
        node: node.clone(),
        port: port.clone(),
        message,
    }
}

impl WebRuntime {
    fn core(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        &mut self.core
    }

    /// A Value input the host may write, and its type.
    fn host_input(&self, node: &NodeId, port: &PortName) -> Result<(InputIdx, Type), RuntimeError> {
        self.core
            .host_input(node, port)
            .map(|(input, ty)| (input, ty.clone()))
    }
}

impl RunningGraph for WebRuntime {
    type Value = JsValue;
    type Handle = JsValue;
    type Reader = WebReader;

    fn reader(&self) -> WebReader {
        self.reader.clone()
    }

    async fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        value: JsValue,
    ) -> Result<(), RuntimeError> {
        // A connected input is refused before its value is looked at.
        let (input, ty) = self.host_input(node, port)?;
        let value = from_js(&value, &ty).map_err(|m| port_error(node, port, m))?;
        check_type(&value, &ty).map_err(|m| port_error(node, port, m))?;
        self.core
            .write_host_input(input, Rc::new(Data::from_value(value, ty)))
            .await
    }

    async fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.host_input(node, port)?;
        let value = parse_wave(&ty, text).map_err(|m| port_error(node, port, m))?;
        self.core
            .write_host_input(input, Rc::new(Data::from_value(value, ty)))
            .await
    }

    async fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.core.clear_host_input(node, port).await
    }

    async fn read_output(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<JsValue>, RuntimeError> {
        self.reader.read_output(node, port).await
    }

    async fn read_output_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        self.reader.read_output_wave(node, port).await
    }

    async fn read_handle(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<JsValue>, RuntimeError> {
        self.reader.read_handle(node, port).await
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.reader.output_version(node, port)
    }

    fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.core().rerun(node)
    }

    fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let core: &SchedulerCore<dyn DataPlane> = &self.core;
        core.node_state(node)
    }

    fn take_faults(&mut self) -> Vec<FaultReport> {
        self.core().take_faults()
    }

    fn shutdown(&mut self) {
        self.core().shutdown();
    }

    async fn tick(&mut self) -> TickResult {
        self.core().tick().await
    }

    async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        self.core().tick_until(stop).await
    }

    async fn snapshot(&mut self) -> Result<Snapshot, SnapshotError> {
        self.core.snapshot_hub().await
    }

    async fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.core.restore_hub(snapshot).await
    }

    fn compiled(&self) -> &CompiledGraph {
        self.core.compiled()
    }
}

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
    /// The name it exports its `node` interface under.
    export: String,
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
                export: found.export,
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
                export: lowered.export,
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

/// The composed component.
struct Composite {
    bytes: Vec<u8>,
    /// Its SHA-256, in hex.
    sha256: String,
}

/// A compiled graph running on this JavaScript host.
#[wasm_bindgen]
pub struct WebGraph {
    /// The scheduler; `None` while a tick has it.
    sched: Rc<RefCell<Option<WebRuntime>>>,
    /// Reads the latched outputs, while a tick holds the scheduler too.
    reader: WebReader,
    /// What the read methods report while a tick has `sched`.
    live: Arc<Live>,
    /// The graph as one component.
    composite: Composite,
    /// What the composite imports, as the host is asked for it.
    capabilities: Vec<Capability>,
    /// The nodes the composite leaves for the host to implement.
    mocked: Vec<NodeId>,
    /// Per node, the stream and future inputs its outputs feed: where the loader
    /// takes each one and gives it back.
    stream_edges: HashMap<NodeId, Vec<(String, PortKind, InputIdx)>>,
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
    /// - `callbacks` is `{ run, rebuild, instance, readHandle, abandon? }`:
    ///   `run(number)` calls the hub's `run` of the node with that number in
    ///   `nodes()` and returns what it returned, or a promise of it;
    ///   `rebuild()` makes a new instance of the composite, throwing or rejecting
    ///   if it cannot (with an error marked `witgraphFatal` when a start function
    ///   called `fatal`); `instance()` returns the live instance's exports,
    ///   `{ control, ports }`; `readHandle(output)` has the hub lend it a handle
    ///   output and returns the object; `abandon()` says the instance stopped, so
    ///   whatever it holds and still does belongs to nothing.
    /// - `mocked` names the nodes left out of the composite, which imports each
    ///   as `m<number>` for the host to implement.
    ///
    /// A graph that does not compile throws with every diagnostic.
    pub fn load(
        graph: &str,
        components: Array,
        callbacks: JsValue,
        mocked: Vec<String>,
    ) -> Result<WebGraph, JsError> {
        let graph: Graph = serde_json::from_str(graph).map_err(error)?;
        let required = |name: &str, key: &str| -> Result<Function, JsError> {
            function(&callbacks, key)?
                .ok_or_else(|| error(format!("`callbacks.{name}` is required")))
        };
        let callbacks = Callbacks {
            run: required("run", "run")?,
            rebuild: required("rebuild", "rebuild")?,
            instance: required("instance", "instance")?,
            read_handle: required("readHandle", "readHandle")?,
            abandon: function(&callbacks, "abandon")?,
        };
        let mocked: Vec<NodeId> = mocked
            .iter()
            .map(|node| NodeId::from(node.as_str()))
            .collect();
        let mut contracts = Vec::new();
        let mut warnings = Vec::new();
        // Per contract, the `components` entry it came from.
        let mut contract_entry = Vec::new();
        let mut exports = Vec::new();
        let mut entries: Vec<(ComponentRef, Vec<u8>)> = Vec::new();
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
                exports.push(lowered.export);
                contract_entry.push(index);
            }
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
        let mut node_exports = HashMap::new();
        for node in compiled.nodes() {
            let contract_at = first_of
                .get(&node.contract.id)
                .copied()
                .ok_or_else(|| error(format!("node `{}` has no component", node.node.id)))?;
            let index = u32::try_from(contract_entry[contract_at]).map_err(error)?;
            node_components.insert(node.node.id.clone(), index);
            node_exports.insert(node.node.id.clone(), exports[contract_at].as_str());
        }
        let linked = link::compose_links(&compiled, &entries, &node_components)?;

        let (composite, capabilities) = compose(
            &compiled,
            &entries,
            &node_components,
            &node_exports,
            &linked.composed,
            &linked.of_node,
            &mocked,
        )?;

        let ports = Rc::new(PortTable::new(&compiled));
        let input_types = IndexVec::from_fn(ports.inputs().len(), |input: InputIdx| {
            value_type(&compiled, &ports.inputs()[input], PortDirection::Input)
        });
        let output_types = IndexVec::from_fn(ports.outputs().len(), |output: OutputIdx| {
            value_type(&compiled, &ports.outputs()[output], PortDirection::Output)
        });
        let stream_edges = stream_inputs(&ports, &compiled);
        let hub = WebHub::new(callbacks, ports, input_types, output_types);
        let live = Live::new(TRACE_LIMIT);
        let mode = Arc::new(LiveMode(live.clone()));
        let plane_hub = hub.clone();
        let core = SchedulerCore::new(
            compiled,
            mode,
            witgraph_sched::DEFAULT_MAX_STEPS_PER_TICK,
            |_, _| WebPlane(plane_hub),
        )
        .map_err(error)?;
        let reader = WebReader {
            hub: hub.clone(),
            versions: core.versions().clone(),
        };
        let sched = WebRuntime {
            core,
            reader: reader.clone(),
        };
        refresh(&sched, &live);
        let (alive, freed) = oneshot::channel();
        Ok(Self {
            sched: Rc::new(RefCell::new(Some(sched))),
            reader,
            live,
            composite,
            capabilities,
            mocked,
            stream_edges,
            warnings,
            _alive: alive,
            freed: freed.shared(),
        })
    }

    /// Runs one iteration of the graph (see `SchedulerCore::tick`). Resolves to
    /// `{ result }`, one of `progress`, `idle`, `interrupted`,
    /// `step-limit`, or `faulted` with `fault` and, when it is known, `node`. When `stop` (a
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
        let live = self.live.clone();
        let freed = self.freed.clone().map(|_| ());
        Ok(future_to_promise(async move {
            let stop = match stop {
                Some(stop) => Either::Left(futures::future::select(stop, freed).map(|_| ())),
                None => Either::Right(freed),
            };
            let result = sched.tick_until(stop).await;
            reconcile(&sched, &live);
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
                TickResult::Faulted(report) => {
                    if let Some(culprit) = &report.culprit {
                        set(&object, "node", &text(culprit.as_str()));
                    }
                    set(&object, "fault", &fault_object(&report.fault));
                    "faulted"
                }
            };
            set(&object, "result", &text(name));
            Ok(object.into())
        }))
    }

    /// The scheduler, unless a tick has it.
    fn idle(&self) -> Result<RefMut<'_, WebRuntime>, JsError> {
        let slot = self.sched.try_borrow_mut().map_err(|_| busy())?;
        RefMut::filter_map(slot, Option::as_mut).map_err(|_| busy())
    }

    /// The scheduler to read from, unless a tick has it.
    fn readable(&self) -> Option<Ref<'_, WebRuntime>> {
        Ref::filter_map(self.sched.try_borrow().ok()?, Option::as_ref).ok()
    }

    /// The graph as one component: `{ bytes, sha256 }`.
    pub fn composite(&self) -> JsValue {
        let object = Object::new();
        set(&object, "sha256", &text(&self.composite.sha256));
        set(
            &object,
            "bytes",
            &Uint8Array::from(self.composite.bytes.as_slice()),
        );
        object.into()
    }

    /// What each of the composite's imports is, by the name `jco transpile` lists
    /// it under: `{ kind: 'host', node }` for a node's `witgraph:runtime/host`;
    /// `{ kind: 'lend' }` for what lends the host a handle; `{ kind: 'mock', node }`
    /// for a mocked node's `node` interface; or `{ kind: 'capability', capability }`,
    /// `{ interface, implements?, items? }` as a wasmtime host is asked for it
    /// (`interface` the full id, version included, or the label, with `implements`
    /// the interface the label stands for). An import that is none of them throws.
    pub fn imports(&self, names: Vec<String>) -> Result<Array, JsError> {
        let sched = self.idle()?;
        let order = sched.compiled().topological_order();
        names
            .iter()
            .map(|name| -> Result<JsValue, JsError> {
                let object = Object::new();
                let number = |prefix: &str, suffix: &str| {
                    name.strip_prefix(prefix)
                        .and_then(|rest| rest.strip_suffix(suffix))
                        .and_then(|at| at.parse::<usize>().ok())
                        .and_then(|at| order.get(at))
                };
                if let Some(node) = number("node-", "-host") {
                    set(&object, "kind", &text("host"));
                    set(&object, "node", &text(node.as_str()));
                    return Ok(object.into());
                }
                if let Some(node) = number("m", "").filter(|node| self.mocked.contains(node)) {
                    set(&object, "kind", &text("mock"));
                    set(&object, "node", &text(node.as_str()));
                    return Ok(object.into());
                }
                if unversioned(name) == unversioned(hub::LEND_INTERFACE) {
                    set(&object, "kind", &text("lend"));
                    return Ok(object.into());
                }
                let named = |by: fn(&str) -> &str| -> Vec<&Capability> {
                    self.capabilities
                        .iter()
                        .filter(|c| by(c.link_name()) == name)
                        .collect()
                };
                let mut found = named(|n| n);
                if found.is_empty() {
                    found = named(unversioned);
                }
                match found.as_slice() {
                    [capability] => {
                        set(&object, "kind", &text("capability"));
                        set(&object, "capability", &json(capability)?);
                        Ok(object.into())
                    }
                    [] => Err(error(format!(
                        "the graph imports `{name}`, which no node's contract lists"
                    ))),
                    many => Err(error(format!(
                        "the graph imports `{name}`, which several capabilities match: {}",
                        many.iter()
                            .map(|c| format!("`{}`", c.interface))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                }
            })
            .collect()
    }

    /// The stream and future inputs a node's outputs feed: an array of
    /// `{ port, kind, input }`, the output port, its kind, and the input's number
    /// for `take-stream` and `give-stream`.
    #[wasm_bindgen(js_name = streamEdges)]
    pub fn stream_edges(&self, node: &str) -> Array {
        self.stream_edges
            .get(&NodeId::from(node))
            .into_iter()
            .flatten()
            .map(|(port, kind, input)| {
                let object = Object::new();
                set(&object, "port", &text(port));
                set(
                    &object,
                    "kind",
                    &text(if *kind == PortKind::Future {
                        "future"
                    } else {
                        "stream"
                    }),
                );
                set(&object, "input", &JsValue::from(input.position() as u32));
                JsValue::from(object)
            })
            .collect()
    }

    /// Reports a trap of the live instance that no call may report: one in a task
    /// a guest spawned, say. It faults the graph, as a `run` rejecting does.
    #[wasm_bindgen(js_name = reportTrap)]
    pub fn report_trap(&self, error: JsValue) {
        self.reader.hub.report_trap(error);
    }

    /// What a mock returned for `node`, each Value output in the form jco lowers:
    /// a mock may write one as WAVE text, or as anything `inject` takes. Stream,
    /// future and handle outputs are left as they are. A value that is no value
    /// of its port throws, naming it.
    #[wasm_bindgen(js_name = mockOutputs)]
    pub fn mock_outputs(&self, node: &str, outputs: JsValue) -> Result<JsValue, JsError> {
        let id = NodeId::from(node);
        let hub = &self.reader.hub;
        let result = Object::new();
        for (output, entry) in hub.ports.outputs().iter_enumerated() {
            if entry.port.node != id {
                continue;
            }
            let name = entry.port.port.as_str();
            let js =
                with_key(name, |key| Reflect::get(&outputs, key)).unwrap_or(JsValue::UNDEFINED);
            let js = match &hub.output_types[output] {
                Some(ty) => Data::from_output(js, ty.clone())
                    .map_err(|e| error(format!("`{node}` returned a bad `{name}`: {e}")))?
                    .js()
                    .clone(),
                None => js,
            };
            set_field(&result, name, &js);
        }
        Ok(result.into())
    }

    /// The nodes left for the host to implement.
    pub fn mocked(&self) -> Array {
        self.mocked.iter().map(|node| text(node.as_str())).collect()
    }

    /// Runs `work` on the scheduler, unless a tick has it. As a JavaScript
    /// `async` function does, it runs at once up to its first wait, so calls
    /// keep their order, and what fails rejects the promise.
    fn on_idle(
        &self,
        work: impl AsyncFnOnce(&mut WebRuntime) -> Result<JsValue, JsError> + 'static,
    ) -> Promise {
        let taken = self
            .sched
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take());
        let Some(mut sched) = taken else {
            return Promise::reject(&JsValue::from(busy()));
        };
        let slot = self.sched.clone();
        settled(async move {
            let result = work(&mut sched).await;
            // Back for the other methods (dropped here if the graph was
            // freed meanwhile).
            slot.replace(Some(sched));
            result
        })
    }

    /// Writes a value, in jco's JavaScript representation, to a Value
    /// input port. Resolves when it is written.
    pub fn inject(&self, node: &str, port: &str, value: JsValue) -> Promise {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        self.on_idle(async move |sched| {
            sched.inject(&node, &port, value).await.map_err(error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Writes a value, as WAVE text, to a Value input port. Resolves when it
    /// is written.
    #[wasm_bindgen(js_name = injectWave)]
    pub fn inject_wave(&self, node: &str, port: &str, wave: String) -> Promise {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        self.on_idle(async move |sched| {
            sched
                .inject_wave(&node, &port, &wave)
                .await
                .map_err(error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Clears an injected Value input. Resolves when it is cleared.
    #[wasm_bindgen(js_name = clearInput)]
    pub fn clear_input(&self, node: &str, port: &str) -> Promise {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        self.on_idle(async move |sched| {
            sched.clear_input(&node, &port).await.map_err(error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Makes the node owe a run on its latched inputs.
    pub fn rerun(&self, node: &str) -> Result<(), JsError> {
        self.idle()?.rerun(&node.into()).map_err(error)
    }

    /// Stops the graph.
    pub fn shutdown(&self) -> Result<(), JsError> {
        self.idle()?.shutdown();
        Ok(())
    }

    /// Which value of a Value or handle output port is latched: a number
    /// that moves on each time the node's `run` writes the port, or a
    /// snapshot is restored. So a preview need read the port only when it
    /// differs from the number it last saw. It works while a tick runs too.
    #[wasm_bindgen(js_name = outputVersion)]
    pub fn output_version(&self, node: &str, port: &str) -> Result<f64, JsError> {
        self.reader
            .output_version(&node.into(), &port.into())
            .map(|version| version.get() as f64)
            .map_err(output_error(node, port))
    }

    /// The latched value of a Value output port, in jco's JavaScript
    /// representation; `undefined` before the node has run. It works while a
    /// tick runs too. The value is the one the node's consumers are given,
    /// not a copy: it must not be changed.
    #[wasm_bindgen(js_name = readOutput)]
    pub fn read_output(&self, node: &str, port: &str) -> Promise {
        let (reader, on_error) = (self.reader.clone(), output_error(node, port));
        let (node, port) = (NodeId::from(node), PortName::from(port));
        settled(async move {
            let value = reader.read_output(&node, &port).await.map_err(on_error)?;
            Ok(value.unwrap_or(JsValue::UNDEFINED))
        })
    }

    /// The latched value of a Value input port, in jco's JavaScript
    /// representation; `undefined` while it has none. It works while a tick runs
    /// too: what a breakpoint shows.
    #[wasm_bindgen(js_name = readInput)]
    pub fn read_input(&self, node: &str, port: &str) -> Promise {
        let reader = self.reader.clone();
        let (node, port) = (NodeId::from(node), PortName::from(port));
        settled(async move {
            let value = reader.input(&node, &port).map_err(error)?;
            Ok(value.unwrap_or(JsValue::UNDEFINED))
        })
    }

    /// The latched object of a handle output port, as its maker returned it;
    /// `undefined` before the node has run. It works while a tick runs too.
    /// It is disposed once nothing in the graph holds it.
    #[wasm_bindgen(js_name = readHandle)]
    pub fn read_handle(&self, node: &str, port: &str) -> Promise {
        let (reader, on_error) = (self.reader.clone(), output_error(node, port));
        let (node, port) = (NodeId::from(node), PortName::from(port));
        settled(async move {
            let value = reader.read_handle(&node, &port).await.map_err(on_error)?;
            Ok(value.unwrap_or(JsValue::UNDEFINED))
        })
    }

    /// The latched value of a Value output port as WAVE text; `undefined`
    /// before the node has run. Rejects, naming the port, for a value too
    /// large to convert (see `readOutput` for the value itself).
    #[wasm_bindgen(js_name = readOutputWave)]
    pub fn read_output_wave(&self, node: &str, port: &str) -> Promise {
        let (reader, on_error) = (self.reader.clone(), output_error(node, port));
        let (node, port) = (NodeId::from(node), PortName::from(port));
        settled(async move {
            let text = reader
                .read_output_wave(&node, &port)
                .await
                .map_err(on_error)?;
            Ok(text.map_or(JsValue::UNDEFINED, |text| text.into()))
        })
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
    /// `{ culprit?, fault }`. A fault stops the whole graph.
    #[wasm_bindgen(js_name = takeFaults)]
    pub fn take_faults(&self) -> Result<Array, JsError> {
        let faults = self.idle()?.take_faults();
        Ok(faults
            .iter()
            .map(|report| {
                let object = Object::new();
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
    pub fn snapshot(&self) -> Promise {
        self.on_idle(async move |sched| {
            let snapshot = sched.snapshot().await.map_err(error)?;
            let text = serde_json::to_string(&snapshot).map_err(error)?;
            Ok(text.into())
        })
    }

    /// Replaces the graph's host-visible state with a snapshot, given as JSON, on
    /// a new instance (`rebuild`).
    pub fn restore(&self, snapshot: String) -> Promise {
        let live = self.live.clone();
        self.on_idle(async move |sched| {
            let snapshot: Snapshot = serde_json::from_str(&snapshot).map_err(error)?;
            sched
                .restore(&snapshot)
                .await
                .map_err(|e: RuntimeError| error(e))?;
            refresh(sched, &live);
            Ok(JsValue::UNDEFINED)
        })
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

    /// The compiled graph's stream groups: arrays of node ids, which always
    /// re-run together, in topological order.
    #[wasm_bindgen(js_name = streamGroups)]
    pub fn stream_groups(&self) -> Result<Array, JsError> {
        let sched = self.idle()?;
        Ok(sched
            .compiled()
            .stream_groups()
            .iter()
            .map(|members| {
                let members: Array = members.iter().map(|m| text(m.as_str())).collect();
                JsValue::from(members)
            })
            .collect())
    }

    /// The compiled graph's nodes, in topological order.
    pub fn nodes(&self) -> Result<Array, JsError> {
        let sched = self.idle()?;
        Ok(sched
            .compiled()
            .topological_order()
            .iter()
            .map(|node| text(node.as_str()))
            .collect())
    }
}

/// The type of a Value port, as its contract has it.
fn value_type(
    compiled: &CompiledGraph,
    entry: &witgraph_ir::PortEntry,
    direction: PortDirection,
) -> Option<Type> {
    let contract = compiled.contract_for(&entry.port.node)?;
    let ports = match direction {
        PortDirection::Input => &contract.inputs,
        PortDirection::Output => &contract.outputs,
    };
    ports
        .iter()
        .find(|p| p.name == entry.port.port && p.kind() == PortKind::Value)
        .and_then(|p| p.ty().cloned())
}

/// Makes `compiled` one component: its hub, and each node with its links composed
/// in, but for the `mocked`. Returns it with what it imports.
fn compose(
    compiled: &CompiledGraph,
    entries: &[(ComponentRef, Vec<u8>)],
    of_node: &HashMap<NodeId, u32>,
    exports: &HashMap<NodeId, &str>,
    composed: &[Composed],
    composed_of: &HashMap<NodeId, usize>,
    mocked: &[NodeId],
) -> Result<(Composite, Vec<Capability>), JsError> {
    let mut capabilities: Vec<Capability> = Vec::new();
    let mut members = Vec::with_capacity(compiled.graph().nodes.len());
    for node in &compiled.graph().nodes {
        let unknown = || error(format!("unknown node `{}`", node.id));
        let contract = compiled.contract_for(&node.id).ok_or_else(unknown)?;
        let is_mocked = mocked.contains(&node.id);
        let linked = composed_of.get(&node.id).map(|at| &composed[*at]);
        let (bytes, export, imports): (&[u8], &str, &[Capability]) = match linked {
            Some(linked) => (&linked.bytes, &linked.export, &linked.capabilities),
            None => {
                let (_, bytes) = of_node
                    .get(&node.id)
                    .and_then(|index| entries.get(*index as usize))
                    .ok_or_else(unknown)?;
                let export = exports.get(&node.id).ok_or_else(unknown)?;
                (bytes, export, &contract.capabilities)
            }
        };
        if !is_mocked {
            Capability::add_unique(&mut capabilities, imports);
        }
        members.push(HubMember {
            node: &node.id,
            bytes,
            export,
            mocked: is_mocked,
        });
    }
    let composed = hub::compose_graph(compiled, &members).map_err(error)?;
    let digest: [u8; 32] = Sha256::digest(&composed.bytes).into();
    Ok((
        Composite {
            bytes: composed.bytes,
            sha256: hex(&digest),
        },
        capabilities,
    ))
}

/// A promise for `work`, which runs at once up to its first wait, as a
/// JavaScript `async` function does.
fn settled(work: impl Future<Output = Result<JsValue, JsError>> + 'static) -> Promise {
    let mut work = Box::pin(work);
    match (&mut work).now_or_never() {
        Some(Ok(value)) => Promise::resolve(&value),
        Some(Err(e)) => Promise::reject(&JsValue::from(e)),
        None => future_to_promise(async move { work.await.map_err(JsValue::from) }),
    }
}

/// The error reading an output port reports.
fn output_error(node: &str, port: &str) -> impl FnOnce(RuntimeError) -> JsError + use<> {
    let (node, port) = (node.to_string(), port.to_string());
    move |e| match e {
        RuntimeError::UnknownNode { .. } => error(format!("unknown node `{node}`")),
        RuntimeError::NotAValuePort { .. } => error(format!(
            "`{node}.{port}` is not an output port of that kind"
        )),
        RuntimeError::ValueType { message, .. } => error(format!("`{node}.{port}`: {message}")),
        other => error(other),
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
/// node's state. At load and on a restore.
fn refresh(sched: &WebRuntime, live: &Live) {
    for node in &sched.compiled().graph().nodes {
        if let Ok(state) = sched.node_state(&node.id) {
            live.set_state(node.id.clone(), live_state(&state));
        }
    }
    live.take_touched();
}

/// Makes exact, from the scheduler, what changed in the live view since it
/// last was: when a tick ends. Only what was touched is copied.
fn reconcile(sched: &WebRuntime, live: &Live) {
    for node in live.take_touched() {
        if let Ok(state) = sched.node_state(&node) {
            live.set_state(node, live_state(&state));
        }
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
    match event {
        TraceEvent::RunStarted { node: id } => {
            named("run-started");
            node(id);
        }
        TraceEvent::RunReturned { node: id } => {
            named("run-returned");
            node(id);
        }
        TraceEvent::GroupDrained { nodes } => {
            named("group-drained");
            let nodes: Array = nodes.iter().map(|node| text(node.as_str())).collect();
            set(&object, "nodes", &nodes);
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
    }
    object.into()
}
