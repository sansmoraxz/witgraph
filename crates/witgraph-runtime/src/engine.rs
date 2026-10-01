//! Wasmtime-based WASM engine.
//!
//! Manages `wasmtime::Engine`, per-node `Store` and `Instance`.
//! Generates typed bindings from the `witgraph:runtime` WIT package via
//! [`wasmtime::component::bindgen!`] and implements the `runtime-host`
//! interface on [`NodeHostState`].

use std::collections::HashMap;

use witgraph_ir::{ComponentRef, PortName};

use crate::abi::{InputSnapshot, OutputCollector, OutputWrite};
use witgraph_ir::Val;

#[allow(missing_docs)]
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "graph-node",
        imports: { default: async },
        exports: { default: async },
    });
}

pub(crate) use bindings::exports::witgraph::runtime::node::Guest as NodeGuest;
pub(crate) use bindings::witgraph::runtime::runtime_host::Host as RuntimeHostTrait;
pub(crate) use bindings::witgraph::runtime::types::Host as TypesHostTrait;
pub(crate) use bindings::witgraph::runtime::types::{
    ActivationKind, ActivationResult as WitActivationResult, DrainItemInfo, PortItemInfo,
};
pub(crate) use bindings::GraphNode;

/// The WASM execution engine wrapping a `wasmtime::Engine`.
///
/// Holds compiled components keyed by [`ComponentRef`] for reuse across
/// nodes that share the same component.
pub struct WasmEngine {
    /// The underlying wasmtime engine.
    pub(crate) engine: wasmtime::Engine,
    /// Compiled components, keyed by component reference.
    pub(crate) compiled: HashMap<ComponentRef, wasmtime::component::Component>,
}

impl WasmEngine {
    /// Creates a new engine, conditionally enabling fuel metering
    /// and epoch-based interruption.
    pub fn new(fuel: bool, epochs: bool) -> Result<Self, wasmtime::Error> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        if fuel {
            config.consume_fuel(true);
        }
        if epochs {
            config.epoch_interruption(true);
        }
        let engine = wasmtime::Engine::new(&config)?;
        Ok(Self {
            engine,
            compiled: HashMap::new(),
        })
    }

    /// Increments the epoch counter, triggering deadline traps in any
    /// store whose deadline has been reached.
    pub fn increment_epoch(&self) {
        self.engine.increment_epoch();
    }

    /// Returns a reference to the underlying wasmtime engine.
    pub fn inner(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Compiles WASM component bytes into a reusable component and
    /// caches the result under the given [`ComponentRef`].
    ///
    /// Returns the compiled component. If the component was previously
    /// compiled, returns the cached version without recompiling.
    pub fn compile(
        &mut self,
        component_ref: &ComponentRef,
        bytes: &[u8],
    ) -> Result<wasmtime::component::Component, wasmtime::Error> {
        if let Some(existing) = self.compiled.get(component_ref) {
            return Ok(existing.clone());
        }
        let component = wasmtime::component::Component::new(&self.engine, bytes)?;
        self.compiled.insert(component_ref.clone(), component.clone());
        Ok(component)
    }

    /// Returns a previously compiled component, if any.
    pub fn get_compiled(
        &self,
        component_ref: &ComponentRef,
    ) -> Option<&wasmtime::component::Component> {
        self.compiled.get(component_ref)
    }
}

/// Per-node WASM component instance with typed bindings.
pub struct NodeInstance {
    /// The typed bindings wrapping the wasmtime component instance.
    pub(crate) bindings: GraphNode,
    /// The per-node store carrying host state.
    pub(crate) store: wasmtime::Store<NodeHostState>,
}

impl NodeInstance {
    /// Creates a new node instance by instantiating a compiled component
    /// with the provided linker.
    ///
    /// The store is initialized with an empty [`NodeHostState`]. When
    /// fuel metering is enabled on the engine, an uncapped fuel budget
    /// is set for instantiation; per-activation budgets are applied
    /// later in [`prepare_activation`](Self::prepare_activation).
    pub async fn instantiate(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        linker: &wasmtime::component::Linker<NodeHostState>,
    ) -> Result<Self, wasmtime::Error> {
        let mut store = wasmtime::Store::new(engine, NodeHostState::new());
        let _ = store.set_fuel(u64::MAX);
        let bindings = GraphNode::instantiate_async(&mut store, component, linker).await?;
        Ok(Self { bindings, store })
    }

    /// Returns a reference to the node's export guest interface.
    pub fn guest(&self) -> &NodeGuest {
        self.bindings.witgraph_runtime_node()
    }

    /// Returns a mutable reference to the per-node wasmtime store.
    pub fn store_mut(&mut self) -> &mut wasmtime::Store<NodeHostState> {
        &mut self.store
    }

    /// Prepares the store for a new activation: installs the input
    /// snapshot, sets resource limits, and marks the cancellation flag.
    /// Returns the previous collector.
    pub fn prepare_activation(
        &mut self,
        snapshot: InputSnapshot,
        fuel: Option<u64>,
        epoch_deadline: Option<u64>,
        cancelled: bool,
    ) -> OutputCollector {
        let state = self.store.data_mut();
        let old_collector = std::mem::replace(&mut state.collector, OutputCollector::new());
        state.snapshot = snapshot;
        state.cancelled = cancelled;
        if let Some(fuel) = fuel {
            let _ = self.store.set_fuel(fuel);
        }
        if let Some(delta) = epoch_deadline {
            self.store.epoch_deadline_trap();
            self.store.set_epoch_deadline(delta);
        }
        old_collector
    }

    /// Extracts the output collector from the store after an activation.
    pub fn take_collector(&mut self) -> OutputCollector {
        std::mem::replace(&mut self.store.data_mut().collector, OutputCollector::new())
    }

    /// Calls the node's `init()` export.
    pub async fn call_init(&mut self) -> Result<(), wasmtime::Error> {
        let guest = self.bindings.witgraph_runtime_node();
        guest.call_init(&mut self.store).await
    }

    /// Calls the node's `activate()` export with the given activation
    /// kind and returns the WIT-level activation result.
    pub async fn call_activate(
        &mut self,
        kind: &ActivationKind,
    ) -> Result<WitActivationResult, wasmtime::Error> {
        let guest = self.bindings.witgraph_runtime_node();
        guest.call_activate(&mut self.store, kind).await
    }

    /// Calls the node's `dispose()` export.
    pub async fn call_dispose(&mut self) -> Result<(), wasmtime::Error> {
        let guest = self.bindings.witgraph_runtime_node();
        guest.call_dispose(&mut self.store).await
    }
}

/// Host-side state attached to each node's wasmtime `Store`.
///
/// Contains the frozen input snapshot for the current activation and
/// the output collector that buffers writes.
pub struct NodeHostState {
    /// The frozen input snapshot for the current activation.
    pub snapshot: InputSnapshot,
    /// The transactional output buffer for the current activation.
    pub collector: OutputCollector,
    /// Whether the host has requested cancellation of this node.
    pub cancelled: bool,
}

impl NodeHostState {
    /// Creates a new host state with an empty snapshot and collector.
    pub fn new() -> Self {
        Self {
            snapshot: InputSnapshot::new(),
            collector: OutputCollector::new(),
            cancelled: false,
        }
    }
}

impl Default for NodeHostState {
    fn default() -> Self {
        Self::new()
    }
}

impl TypesHostTrait for NodeHostState {}

impl RuntimeHostTrait for NodeHostState {
    async fn read_value(&mut self, port: String) -> Option<Vec<u8>> {
        let port_name: PortName = port.into();
        let val = self.snapshot.read_value(&port_name)?;
        match val_to_bytes(val) {
            Ok(bytes) => Some(bytes),
            Err(msg) => {
                self.collector
                    .set_fatal(format!("serialization failed on port `{port_name}`: {msg}"));
                None
            }
        }
    }

    async fn write_value(&mut self, port: String, value: Vec<u8>) {
        match bytes_to_val(&value) {
            Ok(val) => self.collector.write(OutputWrite::Value {
                port: port.into(),
                value: val,
            }),
            Err(msg) => self.collector.set_fatal(
                format!("deserialization failed on port `{port}`: {msg}"),
            ),
        }
    }

    async fn emit_event(&mut self, port: String, payload: Vec<u8>) {
        match bytes_to_val(&payload) {
            Ok(val) => self.collector.write(OutputWrite::Event {
                port: port.into(),
                payload: val,
            }),
            Err(msg) => self.collector.set_fatal(
                format!("deserialization failed on port `{port}`: {msg}"),
            ),
        }
    }

    async fn push_stream(&mut self, port: String, item: Vec<u8>) {
        match bytes_to_val(&item) {
            Ok(val) => self.collector.write(OutputWrite::StreamPush {
                port: port.into(),
                item: val,
            }),
            Err(msg) => self.collector.set_fatal(
                format!("deserialization failed on port `{port}`: {msg}"),
            ),
        }
    }

    async fn close_stream(&mut self, port: String) {
        self.collector.write(OutputWrite::StreamClose {
            port: port.into(),
        });
    }

    async fn resolve_future(&mut self, port: String, value: Vec<u8>) {
        match bytes_to_val(&value) {
            Ok(val) => self.collector.write(OutputWrite::FutureResolve {
                port: port.into(),
                value: val,
            }),
            Err(msg) => self.collector.set_fatal(
                format!("deserialization failed on port `{port}`: {msg}"),
            ),
        }
    }

    async fn fatal(&mut self, message: String) {
        self.collector.set_fatal(message);
    }

    async fn is_cancelled(&mut self) -> bool {
        self.cancelled
    }
}

// ------------------------------------------------------------------
// Val <-> bytes serialization
// ------------------------------------------------------------------

/// Encodes a [`Val`] as a byte vector.
///
/// Uses `serde_json` by default (human-readable, good for debugging).
/// With the `compact-encoding` feature, uses `postcard` (compact binary
/// serde format) for higher throughput.
///
/// Returns an error when the value cannot be serialized (e.g. NaN or
/// Infinity in the `serde_json` path).
pub fn val_to_bytes(val: &Val) -> Result<Vec<u8>, String> {
    #[cfg(feature = "compact-encoding")]
    {
        postcard::to_allocvec(val).map_err(|e| e.to_string())
    }
    #[cfg(not(feature = "compact-encoding"))]
    {
        serde_json::to_vec(val).map_err(|e| e.to_string())
    }
}

/// Decodes a byte vector back into a [`Val`].
///
/// Returns an error when the bytes cannot be deserialized.
fn bytes_to_val(bytes: &[u8]) -> Result<Val, String> {
    #[cfg(feature = "compact-encoding")]
    {
        postcard::from_bytes(bytes).map_err(|e| e.to_string())
    }
    #[cfg(not(feature = "compact-encoding"))]
    {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
}

/// Creates a [`wasmtime::component::Linker`] with the `runtime-host`
/// interface linked for [`NodeHostState`].
pub(crate) fn create_linker(
    engine: &wasmtime::Engine,
) -> Result<wasmtime::component::Linker<NodeHostState>, wasmtime::Error> {
    let mut linker = wasmtime::component::Linker::new(engine);
    GraphNode::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |state| state,
    )?;
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn val_round_trip_bool() {
        let val = Val::Bool(true);
        let bytes = val_to_bytes(&val).unwrap();
        assert_eq!(bytes_to_val(&bytes).unwrap(), val);
    }

    #[test]
    fn val_round_trip_integers() {
        for val in [
            Val::U8(255),
            Val::U16(1000),
            Val::U32(42),
            Val::U64(u64::MAX),
            Val::S8(-128),
            Val::S16(-1),
            Val::S32(i32::MIN),
            Val::S64(0),
        ] {
            let bytes = val_to_bytes(&val).unwrap();
            assert_eq!(bytes_to_val(&bytes).unwrap(), val, "round-trip failed for {val:?}");
        }
    }

    #[test]
    fn val_round_trip_floats() {
        let val = Val::F64(2.5);
        let bytes = val_to_bytes(&val).unwrap();
        assert_eq!(bytes_to_val(&bytes).unwrap(), val);
    }

    #[test]
    fn val_round_trip_string() {
        let val = Val::String("hello \"world\"\nnewline".into());
        let bytes = val_to_bytes(&val).unwrap();
        assert_eq!(bytes_to_val(&bytes).unwrap(), val);
    }

    #[test]
    fn val_round_trip_char() {
        let val = Val::Char('Z');
        let bytes = val_to_bytes(&val).unwrap();
        assert_eq!(bytes_to_val(&bytes).unwrap(), val);
    }

    #[test]
    fn val_round_trip_complex() {
        let vals = [
            Val::List(vec![Val::U32(1), Val::U32(2), Val::U32(3)]),
            Val::Option(Some(Box::new(Val::String("inner".into())))),
            Val::Option(None),
            Val::Tuple(vec![Val::Bool(true), Val::U64(42)]),
            Val::Record(vec![
                ("x".into(), Val::F32(1.0)),
                ("y".into(), Val::F32(2.0)),
            ]),
            Val::Variant {
                case: "ok".into(),
                payload: Some(Box::new(Val::U8(1))),
            },
            Val::Enum("red".into()),
            Val::Flags(vec!["a".into(), "b".into()]),
        ];
        for val in vals {
            let bytes = val_to_bytes(&val).unwrap();
            assert_eq!(bytes_to_val(&bytes).unwrap(), val, "round-trip failed for {val:?}");
        }
    }

    #[test]
    fn bytes_to_val_rejects_invalid() {
        let unknown = b"not json at all";
        let result = bytes_to_val(unknown);
        assert!(result.is_err(), "invalid bytes should be rejected");
    }

    #[test]
    fn host_state_default() {
        let state = NodeHostState::default();
        assert!(state.snapshot.read_value(&"x".into()).is_none());
        assert!(state.collector.is_empty());
        assert!(!state.cancelled);
    }
}
