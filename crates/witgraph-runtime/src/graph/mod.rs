//! The `RuntimeGraph` typestate: a compiled graph composed into one component,
//! ready for execution.
//!
//! Extends the typestate chain: `GraphBuilder -> Graph -> CompiledGraph ->
//! PreparedGraph -> RuntimeGraph`.
//!
//! A `RuntimeGraph` is the witgraph scheduler ([`witgraph_sched`]), which decides
//! when each node runs and holds no value, over a generated hub composed with the
//! nodes ([`witgraph_wit::hub`]), which holds them and moves them. The host reads
//! and writes one port at a time, only when it asks: through a [`Port`] when it
//! knows the type, which typechecks once and calls with no conversion, or through
//! [`Val`] when it does not. The scheduling model, with its invariants, is
//! described in [`witgraph_sched::control`].
//!
//! Reads and writes wait for the guest to suspend when one is spinning, which is
//! forever with no fuel budget. Bound them with [`until`](witgraph_traits::until)
//! if that could matter; giving up on one is safe.

pub(crate) mod load;

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use wasmtime::Engine;
use wasmtime::component::{Accessor, ComponentNamedList, ComponentType, Lift, Lower, TypedFunc};
use wasmtime::component::{Type, Val};
use witgraph_ir::{
    CompiledGraph, Idx, IndexVec, InputIdx, NodeId, OutputIdx, PortDirection, PortKind, PortName,
    PortTable,
};
use witgraph_sched::{
    DataPlane, FaultReport, Incarnation, NodeState, Perf, PortVersion, PortVersions, RuntimeError,
    RuntimeMode, SchedulerCore, TickResult, latched_output, not_a_value_output,
};
use witgraph_traits::{OutputReader, RunningGraph, Snapshot, SnapshotError};

pub use self::load::PreparedGraph;
use crate::engine::{self, Host, NoCapabilities, StoreData, StoreSettings};
use crate::plane::{Client, ClientSlot, HostWrite, HubPlane, LiveHub, ValWrite, call_get, lock};
use crate::shared::Shared;
#[cfg(feature = "compile")]
pub use witgraph_wit::compose::LinkedProvider;

/// An amount of wasmtime's fuel: roughly, of instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Fuel(pub u64);

/// A number of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ByteCount(pub u64);

impl ByteCount {
    /// As a number of bytes in this host's address space, saturating.
    pub fn to_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// Configuration for a runtime graph.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Most `run`s one tick may start before it returns
    /// [`TickResult::StepLimitReached`]: a bound on how long one tick can
    /// keep starting work, not a loop detector. A tick that hits it latches
    /// no feedback, so feedback loops advance at most one iteration per
    /// tick; a driver bounds a loop that never settles by the number of
    /// ticks it runs.
    pub max_steps_per_tick: usize,
    /// Fuel the graph burns before yielding to the executor (at least 1),
    /// so a timeout around [`RuntimeGraph::tick`] fires even while a guest spins.
    pub yield_interval: Fuel,
    /// Fuel the graph may burn between two `run` starts before it faults
    /// with [`NodeFault::FuelExhausted`](crate::NodeFault::FuelExhausted):
    /// the fuel is reset to this budget before every `run` call, before every
    /// host read, write or feedback latch made while no guest is at work, and
    /// instantiating the graph (start functions included) gets a budget of the
    /// same size. The budget is shared by everything executing in the graph,
    /// spawned tasks and a host's calls made while a guest is at work included, so
    /// an endless stream exhausts any finite budget eventually. `None` means
    /// effectively unlimited, instantiation included.
    pub fuel_per_run: Option<Fuel>,
    /// Bytes of linear memory and tables (8 bytes per element) the graph may
    /// hold in total. Growing past it faults the graph
    /// ([`NodeFault::MemoryLimit`](crate::NodeFault::MemoryLimit)). `None` is
    /// unlimited. The graph may also hold at most 16 memories, 16 tables and 64
    /// core instances per component composed into it (a node, a provider, and
    /// the hub).
    pub max_memory: Option<ByteCount>,
    /// The most memory the graph's shared objects, the resources its
    /// handle ports carry ([`shared`](crate::shared)), may take in total,
    /// as their providers report their sizes (host or device memory).
    /// Making one past it fails the call that makes it, which faults the
    /// graph ([`NodeFault::MemoryLimit`](crate::NodeFault::MemoryLimit)); an
    /// object's size is given back when the last holder drops it. `None` is
    /// unlimited; the default is 4 GiB.
    pub max_shared_memory: Option<ByteCount>,
    /// Wasmtime's hostcall fuel, per call: roughly the bytes of host memory
    /// the values lifted *out of* a guest in one call may take, where every
    /// value element costs about 48 bytes. It bounds what a dynamic read returns,
    /// and the arguments a guest passes to an imported function (a capability, or
    /// `fatal`). Going over faults the graph, a read's too
    /// ([`NodeFault::HostcallFuelExhausted`](crate::NodeFault::HostcallFuelExhausted)):
    /// the engine stops the Store, so bind a large port with
    /// [`output`](RuntimeGraph::output), or read it with
    /// [`preview`](RuntimeGraph::preview) if it is a `list<u8>`.
    /// Values passed *into* a guest, and values moving between nodes, are not
    /// charged.
    ///
    /// The default, 4 GiB (everything a 32-bit host addresses, on one), lets a
    /// dynamic read of a `list<u8>` of about 85 MiB through; a typed read
    /// ([`RuntimeGraph::get`]) costs far less. Lower it to bound what one call may
    /// make the host allocate.
    pub hostcall_fuel: usize,
    /// How much address space each linear memory reserves up front, rounded
    /// up to whole 64 KiB pages (`0` reserves nothing). `None` keeps
    /// wasmtime's default (4 GiB plus guards on 64-bit hosts): compiled
    /// code then skips bounds checks, but a process holds at most about 32k
    /// memories. A smaller reservation holds more memories at the cost of
    /// bounds-checked loads and stores; a memory that outgrows its
    /// reservation moves. Only used when the runtime makes the engine
    /// ([`engine`](Self::engine) is `None` and no prepared graph brings one).
    pub memory_reservation: Option<ByteCount>,
    /// Lets guests built with debug info be stopped on a line of their source:
    /// [`RuntimeGraph::lines`]. Wasmtime then keeps every value where it can be looked at, so
    /// guests run about 20 times slower, breakpoints or not. It needs an engine from
    /// [`new_engine`](Self::new_engine) with this set, as the default one is. What it says is
    /// which of a guest's lines count as its own.
    #[cfg(feature = "debug")]
    pub debug_lines: Option<witgraph_traits::lines::OwnLines>,
    /// The wasmtime engine to load the graph into: one from
    /// [`RuntimeConfig::new_engine`], or another graph's
    /// [`engine`](RuntimeGraph::engine), so several graphs share one. It is
    /// checked at load: it must meter fuel, support the component model's
    /// async ABI, value types and concurrency, have no epoch interruption,
    /// and no shared memories (which the memory limit cannot see). `None`
    /// uses the engine a prepared graph was compiled on, or else makes
    /// one. Either way, a loaded graph's [`config`](RuntimeGraph::config)
    /// holds the engine it runs on.
    pub engine: Option<Engine>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps_per_tick: witgraph_sched::DEFAULT_MAX_STEPS_PER_TICK,
            yield_interval: Fuel(100_000),
            fuel_per_run: None,
            max_memory: Some(ByteCount(4 << 30)),
            max_shared_memory: Some(ByteCount(4 << 30)),
            // 4 GiB, or all of a 32-bit host's address space.
            hostcall_fuel: usize::try_from(4u64 << 30).unwrap_or(usize::MAX),
            memory_reservation: None,
            #[cfg(feature = "debug")]
            debug_lines: None,
            engine: None,
        }
    }
}

impl RuntimeConfig {
    /// A wasmtime engine set up the way the runtime needs (the component
    /// model's async ABI and value types, fuel metering), with
    /// [`memory_reservation`](Self::memory_reservation).
    pub fn new_engine(&self) -> Result<Engine, RuntimeError> {
        let memory_reservation = self.memory_reservation.map(|bytes| bytes.0);
        #[cfg(feature = "debug")]
        let made = if self.debug_lines.is_some() {
            engine::new_debug_engine(memory_reservation)
        } else {
            engine::new_engine(memory_reservation)
        };
        #[cfg(not(feature = "debug"))]
        let made = engine::new_engine(memory_reservation);
        made.map_err(|e| RuntimeError::InvalidConfig {
            message: format!("{e:#}"),
        })
    }

    fn store_settings(&self) -> StoreSettings {
        StoreSettings {
            yield_interval: self.yield_interval.0,
            fuel_per_run: self.fuel_per_run.map(|fuel| fuel.0),
            hostcall_fuel: self.hostcall_fuel,
        }
    }
}

/// The types of a graph's Value ports, from its hub's functions.
#[derive(Debug, Clone)]
pub(crate) struct PortTypes {
    pub(crate) inputs: IndexVec<InputIdx, Option<Type>>,
    pub(crate) outputs: IndexVec<OutputIdx, Option<Type>>,
}

/// A compiled graph composed and loaded, ready for execution.
///
/// `M` is the instrumentation mode; `H` is the embedder's [`Host`], which
/// provides capability imports and the data of the Store.
pub struct RuntimeGraph<M: RuntimeMode = Perf, H: Host = NoCapabilities> {
    mode: Arc<M>,
    core: SchedulerCore<HubPlane<H>>,
    config: RuntimeConfig,
    /// Tells a [`Port`] of this graph from one of another.
    id: GraphId,
}

/// Which graph a [`Port`] was bound on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct GraphId(u64);

impl GraphId {
    fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// [`Port`]'s direction: written by the host.
#[derive(Debug, Clone, Copy)]
pub struct Input;
/// [`Port`]'s direction: read by the host.
#[derive(Debug, Clone, Copy)]
pub struct Output;
/// [`Port`]'s kind: a value, which the hub compares and copies.
#[derive(Debug, Clone, Copy)]
pub struct Value;
/// [`Port`]'s kind: a resource handle, which the hub lends.
#[derive(Debug, Clone, Copy)]
pub struct Handle;

/// A port bound to a Rust type: an input to write ([`RuntimeGraph::set`]), or an
/// output to read ([`RuntimeGraph::get`], [`RuntimeGraph::get_handle`]). The
/// engine checks the type when it is bound, and again only after a rebuild, so a
/// call does no name lookup and no conversion through [`Val`].
pub struct Port<D, K, T: 'static, F = ()> {
    graph: GraphId,
    index: usize,
    node: NodeId,
    port: PortName,
    /// The bound function, and the instance it belongs to.
    bound: Mutex<Option<(Incarnation, F)>>,
    marker: PhantomData<fn(D, K) -> T>,
}

/// A typed write to an input.
pub type InputPort<T> = Port<Input, Value, T, TypedFunc<(Vec<T>,), (bool,)>>;
/// A typed read of an output.
pub type OutputPort<T> = Port<Output, Value, T, TypedFunc<(), (Option<T>,)>>;
/// A read of a handle output.
pub type HandlePort = Port<Output, Handle, Shared>;

impl<D, K, T: 'static, F> Port<D, K, T, F> {
    fn new(graph: GraphId, index: usize, node: NodeId, port: PortName) -> Self {
        Self {
            graph,
            index,
            node,
            port,
            bound: Mutex::new(None),
            marker: PhantomData,
        }
    }

    /// The node the port belongs to.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The port's name.
    pub fn name(&self) -> &PortName {
        &self.port
    }
}

impl<D, K, T: 'static, F: Copy> Port<D, K, T, F> {
    fn bound_in(&self, incarnation: Incarnation) -> Option<F> {
        lock(&self.bound)
            .as_ref()
            .filter(|(of, _)| *of == incarnation)
            .map(|(_, func)| *func)
    }

    fn bind(&self, incarnation: Incarnation, func: F) {
        *lock(&self.bound) = Some((incarnation, func));
    }
}

/// A value read as what suits it best.
#[derive(Debug, Clone, PartialEq)]
pub enum Preview {
    /// A `list<u8>`, as bytes: a typed read, with no [`Val`] per element.
    Bytes(Vec<u8>),
    /// Anything else, as a dynamic value.
    Val(Val),
}

/// What reads a graph's outputs, whatever its Store's data.
trait ReadOutputs: Send + Sync + 'static {
    fn read_val(&self, output: OutputIdx) -> BoxFuture<'static, Result<Option<Val>, RuntimeError>>;
    fn read_shared(
        &self,
        output: OutputIdx,
    ) -> BoxFuture<'static, Result<Option<Shared>, RuntimeError>>;
}

struct Outputs<D: StoreData>(ClientSlot<D>);

impl<D: StoreData> ReadOutputs for Outputs<D> {
    fn read_val(&self, output: OutputIdx) -> BoxFuture<'static, Result<Option<Val>, RuntimeError>> {
        let client = lock(&self.0).clone();
        Box::pin(async move {
            let Some(client) = client else {
                return Ok(None);
            };
            let read = client.call(move |acc, hub| {
                Box::pin(async move { call_get(acc, hub.get_output(acc, output)?).await })
            });
            stopped_is_none(read.await)
        })
    }

    fn read_shared(
        &self,
        output: OutputIdx,
    ) -> BoxFuture<'static, Result<Option<Shared>, RuntimeError>> {
        let client = lock(&self.0).clone();
        Box::pin(async move {
            let Some(client) = client else {
                return Ok(None);
            };
            let read = client
                .call(move |acc, hub| Box::pin(async move { hub.read_handle(acc, output).await }));
            stopped_is_none(read.await)
        })
    }
}

/// A stopped graph holds nothing to read.
fn stopped_is_none<T>(read: Result<Option<T>, RuntimeError>) -> Result<Option<T>, RuntimeError> {
    match read {
        Err(RuntimeError::InstanceStopped) => Ok(None),
        read => read,
    }
}

/// Reads a [`RuntimeGraph`]'s latched outputs, also while it ticks.
#[derive(Clone)]
pub struct ValReader {
    outputs: Arc<dyn ReadOutputs>,
    ports: Arc<PortTable>,
    versions: PortVersions,
}

impl ValReader {
    /// Checks that `node` exists and `port` is an output of it of a latched kind.
    fn latched(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(OutputIdx, PortKind), RuntimeError> {
        latched_output(&self.ports, node, port)
    }
}

impl OutputReader for ValReader {
    type Value = Val;
    type Handle = Shared;

    async fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Val>, RuntimeError> {
        match self.latched(node, port)? {
            (output, PortKind::Value) => self.outputs.read_val(output).await,
            _ => Err(not_a_value_output(node, port)),
        }
    }

    async fn read_output_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        let Some(val) = self.read_output(node, port).await? else {
            return Ok(None);
        };
        val.to_wave()
            .map(Some)
            .map_err(|e| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message: e.to_string(),
            })
    }

    async fn read_handle(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Shared>, RuntimeError> {
        match self.latched(node, port)? {
            (output, PortKind::Handle) => self.outputs.read_shared(output).await,
            _ => Err(not_a_value_output(node, port)),
        }
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.latched(node, port)
            .map(|(output, _)| self.versions.get(output))
    }
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    fn core(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        &mut self.core
    }

    fn client(&self) -> Option<Client<H::Data>> {
        lock(&self.core.plane.client).clone()
    }

    /// Binds a Value input to `T`, the type of its payload (for an optional
    /// port, the inner type), which the engine checks
    /// ([`RuntimeError::ValueType`]). The same ports as
    /// [`inject`](RunningGraph::inject) qualify. It needs a live graph
    /// ([`RuntimeError::InstanceStopped`]).
    pub async fn input<T>(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<InputPort<T>, RuntimeError>
    where
        T: ComponentType + Lower + Clone + Send + Sync + 'static,
        (Vec<T>,): ComponentNamedList + Lower,
    {
        let (input, _) = self.core.host_input(node, port)?;
        let bound = Port::new(self.id, input.position(), node.clone(), port.clone());
        self.bind_input(&bound, input).await?;
        Ok(bound)
    }

    async fn bind_input<T>(
        &self,
        port: &InputPort<T>,
        input: InputIdx,
    ) -> Result<TypedFunc<(Vec<T>,), (bool,)>, RuntimeError>
    where
        T: ComponentType + Lower + Clone + Send + Sync + 'static,
        (Vec<T>,): ComponentNamedList + Lower,
    {
        let client = self.client().ok_or(RuntimeError::InstanceStopped)?;
        if let Some(func) = port.bound_in(client.incarnation()) {
            return Ok(func);
        }
        let func = client
            .call(move |acc, hub| {
                Box::pin(async move {
                    let set = hub.set_input(acc, input)?;
                    acc.with(|access| {
                        set.typed::<(Vec<T>,), (bool,)>(access)
                            .map(Ok::<_, String>)
                            .or_else(|e| Ok(Err(format!("{e:#}"))))
                    })
                })
            })
            .await?
            .map_err(|message| RuntimeError::ValueType {
                node: port.node.clone(),
                port: port.port.clone(),
                message,
            })?;
        port.bind(client.incarnation(), func);
        Ok(func)
    }

    /// Writes a value through a port bound with [`input`](Self::input): as
    /// [`inject`](RunningGraph::inject), with no conversion.
    pub async fn set<T>(&mut self, port: &InputPort<T>, value: T) -> Result<(), RuntimeError>
    where
        T: ComponentType + Lower + Clone + Send + Sync + 'static,
        (Vec<T>,): ComponentNamedList + Lower,
    {
        self.check_graph(port.graph)?;
        let input = InputIdx::from_position(port.index);
        let write: Arc<dyn HostWrite<H::Data>> = Arc::new(TypedWrite(value));
        if self.client().is_some() {
            self.bind_input(port, input).await?;
        }
        self.core.write_host_input(input, write).await
    }

    /// Binds a Value output to `T`, which the engine checks
    /// ([`RuntimeError::ValueType`]). It needs a live graph
    /// ([`RuntimeError::InstanceStopped`]).
    pub async fn output<T>(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<OutputPort<T>, RuntimeError>
    where
        T: ComponentType + Lift + Send + Sync + 'static,
        (Option<T>,): ComponentNamedList + Lift,
    {
        let (output, kind) = self.reader().latched(node, port)?;
        if kind != PortKind::Value {
            return Err(not_a_value_output(node, port));
        }
        let bound = Port::new(self.id, output.position(), node.clone(), port.clone());
        self.bind_output(&bound).await?;
        Ok(bound)
    }

    async fn bind_output<T>(
        &self,
        port: &OutputPort<T>,
    ) -> Result<TypedFunc<(), (Option<T>,)>, RuntimeError>
    where
        T: ComponentType + Lift + Send + Sync + 'static,
        (Option<T>,): ComponentNamedList + Lift,
    {
        let client = self.client().ok_or(RuntimeError::InstanceStopped)?;
        if let Some(func) = port.bound_in(client.incarnation()) {
            return Ok(func);
        }
        let output = OutputIdx::from_position(port.index);
        let func = client
            .call(move |acc, hub| {
                Box::pin(async move {
                    let get = hub.get_output(acc, output)?;
                    acc.with(|access| {
                        get.typed::<(), (Option<T>,)>(access)
                            .map(Ok::<_, String>)
                            .or_else(|e| Ok(Err(format!("{e:#}"))))
                    })
                })
            })
            .await?
            .map_err(|message| RuntimeError::ValueType {
                node: port.node.clone(),
                port: port.port.clone(),
                message,
            })?;
        port.bind(client.incarnation(), func);
        Ok(func)
    }

    /// Reads a value through a port bound with [`output`](Self::output): what the
    /// node's `run` returned last, or `None` if it has not run since the graph
    /// was built, or the graph is stopped.
    pub async fn get<T>(&self, port: &OutputPort<T>) -> Result<Option<T>, RuntimeError>
    where
        T: ComponentType + Lift + Send + Sync + 'static,
        (Option<T>,): ComponentNamedList + Lift,
    {
        self.check_graph(port.graph)?;
        let Some(client) = self.client() else {
            return Ok(None);
        };
        let get = match self.bind_output(port).await {
            Err(RuntimeError::InstanceStopped) => return Ok(None),
            bound => bound?,
        };
        let read = client
            .call(move |acc, _| Box::pin(async move { Ok(get.call_concurrent(acc, ()).await?.0) }))
            .await;
        stopped_is_none(read)
    }

    /// Binds a handle output.
    pub fn handle_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<HandlePort, RuntimeError> {
        let (output, kind) = self.reader().latched(node, port)?;
        if kind != PortKind::Handle {
            return Err(not_a_value_output(node, port));
        }
        Ok(Port::new(
            self.id,
            output.position(),
            node.clone(),
            port.clone(),
        ))
    }

    /// Reads a handle's object through a port bound with
    /// [`handle_output`](Self::handle_output), or `None` if the node has not run
    /// since the graph was built, or the graph is stopped.
    pub async fn get_handle(&self, port: &HandlePort) -> Result<Option<Shared>, RuntimeError> {
        self.check_graph(port.graph)?;
        let reader = self.reader();
        reader
            .outputs
            .read_shared(OutputIdx::from_position(port.index))
            .await
    }

    fn check_graph(&self, graph: GraphId) -> Result<(), RuntimeError> {
        if graph == self.id {
            return Ok(());
        }
        Err(RuntimeError::InvalidConfig {
            message: "a port bound on another graph".into(),
        })
    }

    /// Reads an output as what suits it: a `list<u8>` as bytes, with a typed
    /// call, and anything else as a [`Val`]. So a host looking at one every 30
    /// seconds pays when it looks, and pays the least the type allows.
    pub async fn preview(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Preview>, RuntimeError> {
        let reader = self.reader();
        let (output, kind) = reader.latched(node, port)?;
        if kind != PortKind::Value {
            return Err(not_a_value_output(node, port));
        }
        let bytes =
            matches!(self.output_type(node, port)?, Type::List(list) if list.ty() == Type::U8);
        if !bytes {
            let read = reader.outputs.read_val(output).await?;
            return Ok(read.map(Preview::Val));
        }
        let Some(client) = self.client() else {
            return Ok(None);
        };
        let read = client
            .call(move |acc, hub| {
                Box::pin(async move {
                    let get = hub.get_output(acc, output)?;
                    let get = acc.with(|access| get.typed::<(), (Option<Vec<u8>>,)>(access))?;
                    Ok(get.call_concurrent(acc, ()).await?.0)
                })
            })
            .await;
        Ok(stopped_is_none(read)?.map(Preview::Bytes))
    }

    /// Reads an output as WAVE text.
    pub async fn preview_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        self.reader().read_output_wave(node, port).await
    }

    /// The memory the graph's shared objects take now, as their providers
    /// reported it: what counts against
    /// [`max_shared_memory`](RuntimeConfig::max_shared_memory).
    pub fn shared_memory(&self) -> usize {
        self.core.plane.blueprint.shared_memory.used()
    }

    /// The component type of a Value input port's payload (for an optional
    /// port, its inner type): what [`inject`](RunningGraph::inject) checks values
    /// against. Any Value input qualifies, connected or not.
    pub fn input_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        let ports = self.core.ports();
        ports
            .node(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        ports
            .input(node, port)
            .and_then(|input| self.core.plane.types.inputs[input].as_ref())
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction: PortDirection::Input,
            })
    }

    /// The component type of a Value output port.
    pub fn output_type(&self, node: &NodeId, port: &PortName) -> Result<&Type, RuntimeError> {
        let ports = self.core.ports();
        ports
            .node(node)
            .ok_or_else(|| RuntimeError::UnknownNode { node: node.clone() })?;
        ports
            .output(node, port)
            .and_then(|output| self.core.plane.types.outputs[output].as_ref())
            .ok_or_else(|| not_a_value_output(node, port))
    }

    /// The runtime configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// What sets breakpoints in the graph's guests and steps through them, if it was loaded
    /// with [`RuntimeConfig::debug_lines`].
    #[cfg(feature = "debug")]
    pub fn lines(&self) -> Option<crate::lines::RuntimeLines> {
        self.core.plane.blueprint.lines.clone()
    }

    /// What holds this graph's node runs before they start, and lets them go: a debugger's
    /// node breakpoints and stepping. Clone it to hold on to it while a tick runs. Nothing is
    /// held until it is asked to.
    pub fn holds(&self) -> crate::holds::RuntimeHolds {
        self.core.plane.blueprint.holds.clone()
    }

    /// The wasmtime engine the graph runs on; pass it as
    /// [`RuntimeConfig::engine`] to load another graph on it.
    pub fn engine(&self) -> &Engine {
        &self.core.plane.blueprint.engine
    }

    /// The embedder's host.
    pub fn host(&self) -> &H {
        &self.core.plane.blueprint.host
    }

    /// The embedder's host, to change capability state it shares with
    /// future Stores.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.core.plane.blueprint.host
    }
}

/// A typed value written to an input, kept to write again after a rebuild.
struct TypedWrite<T>(T);

impl<D, T> HostWrite<D> for TypedWrite<T>
where
    D: StoreData,
    T: ComponentType + Lower + Clone + Send + Sync + 'static,
    (Vec<T>,): ComponentNamedList + Lower,
{
    fn write<'a>(
        &'a self,
        acc: &'a Accessor<D>,
        hub: &'a LiveHub,
        input: InputIdx,
    ) -> Pin<Box<dyn Future<Output = wasmtime::Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            let set = hub.set_input(acc, input)?;
            let set = acc.with(|access| set.typed::<(Vec<T>,), (bool,)>(access))?;
            Ok(set.call_concurrent(acc, (vec![self.0.clone()],)).await?.0)
        })
    }

    fn wave(&self) -> Option<Result<String, String>> {
        None
    }

    fn val(&self) -> Option<&Val> {
        None
    }
}

impl<M: RuntimeMode, H: Host> RunningGraph for RuntimeGraph<M, H> {
    type Value = Val;
    type Handle = Shared;
    type Reader = ValReader;

    fn reader(&self) -> ValReader {
        ValReader {
            outputs: Arc::new(Outputs(self.core.plane.client.clone())),
            ports: self.core.ports().clone(),
            versions: self.core.versions().clone(),
        }
    }

    async fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        value: Val,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.core.host_input(node, port)?;
        engine::check_type(ty, &value).map_err(|message| RuntimeError::ValueType {
            node: node.clone(),
            port: port.clone(),
            message,
        })?;
        let value = engine::canonical(ty, value);
        self.core
            .write_host_input(input, Arc::new(ValWrite(value)))
            .await
    }

    async fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.core.host_input(node, port)?;
        let value = engine::parse_wave(ty, text).map_err(|message| RuntimeError::ValueType {
            node: node.clone(),
            port: port.clone(),
            message,
        })?;
        self.core
            .write_host_input(input, Arc::new(ValWrite(value)))
            .await
    }

    async fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.core.clear_host_input(node, port).await
    }

    async fn read_output(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Val>, RuntimeError> {
        self.reader().read_output(node, port).await
    }

    async fn read_output_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<String>, RuntimeError> {
        self.reader().read_output_wave(node, port).await
    }

    async fn read_handle(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<Shared>, RuntimeError> {
        self.reader().read_handle(node, port).await
    }

    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError> {
        self.reader().output_version(node, port)
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

    fn catch_up(&mut self) {
        self.core().catch_up();
    }

    fn shutdown(&mut self) {
        self.core().shutdown();
    }

    async fn tick(&mut self) -> TickResult {
        self.core().tick().await
    }

    async fn tick_until(&mut self, stop: impl Future<Output = ()> + Send) -> TickResult {
        self.core().tick_until(stop).await
    }

    /// See [`SchedulerCore::snapshot_hub`]. One written through a typed [`Port`] has
    /// no text ([`SnapshotError`]) while the graph is stopped.
    async fn snapshot(&mut self) -> Result<Snapshot, SnapshotError> {
        self.core.snapshot_hub().await
    }

    /// See [`SchedulerCore::restore_hub`].
    async fn restore(&mut self, snapshot: &Snapshot) -> Result<(), RuntimeError> {
        self.core.restore_hub(snapshot).await
    }

    fn compiled(&self) -> &CompiledGraph {
        self.core.compiled()
    }
}
