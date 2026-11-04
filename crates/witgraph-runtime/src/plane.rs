//! The data plane: one long-lived [`Store::run_concurrent`] serving a composed
//! graph's hub.
//!
//! What the scheduler asks ([`Command`]) and what the host pulls (a [`Request`])
//! all reach one server future, which owns the Store. `run` calls overlap, as
//! the hub's `run` is async. Everything else goes through one lane, in the order
//! it was asked, so a write is seen by what was asked after it.
//!
//! The server future is [`Shared`](futures::future::Shared): the scheduler polls
//! it in a tick, and a host waiting on a pull polls it too, so a pull is answered
//! with no tick running, and every poller is woken.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use futures::FutureExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{Either, Shared as SharedFuture};
use futures::stream::{FuturesUnordered, StreamExt};
use wasmtime::component::{
    Accessor, Component, ComponentExportIndex, ComponentType, Func, Instance, InstancePre, Lift,
    Type, TypedFunc, Val,
};
use wasmtime::{AsContextMut, Engine, Store};
use witgraph_ir::{FeedbackIdx, Idx, IndexVec, InputIdx, NodeId, NodeIdx, OutputIdx, PortTable};
use witgraph_sched::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, HubPorts, Latched, NodeFault,
    Outcome, PortSlot, Returned, RuntimeError, Serving, boxed,
};
use witgraph_wit::hub;

use crate::engine::{self, Host, HostState, StoreData, StoreSettings, classify};
use crate::graph::PortTypes;
use crate::shared::{Shared, SharedMemory};

/// `witgraph:hub/control`'s `returned`, as the guest numbers things.
#[derive(ComponentType, Lift, Debug, Clone, Default)]
#[component(record)]
struct RawReturned {
    #[component(name = "outputs-changed")]
    outputs_changed: Vec<u32>,
    #[component(name = "inputs-changed")]
    inputs_changed: Vec<u32>,
    #[component(name = "inputs-recorded")]
    inputs_recorded: Vec<u32>,
    #[component(name = "inputs-cleared")]
    inputs_cleared: Vec<u32>,
    #[component(name = "feedback-buffered")]
    feedback_buffered: Vec<u32>,
}

/// `witgraph:hub/control`'s `latched`.
#[derive(ComponentType, Lift, Debug, Clone, Copy)]
#[component(enum)]
#[repr(u8)]
#[expect(dead_code, reason = "only ever lifted from the guest")]
enum RawLatched {
    #[component(name = "unchanged")]
    Unchanged,
    #[component(name = "changed")]
    Changed,
    #[component(name = "cleared")]
    Cleared,
}

/// Numbers from a hub as numbers of `len` ports. The one place they are trusted:
/// nothing else sees a raw one.
fn decode<I: Idx>(raw: Vec<u32>, len: usize) -> wasmtime::Result<Vec<I>> {
    raw.into_iter()
        .map(|at| {
            let at = at as usize;
            if at >= len {
                wasmtime::bail!("the hub named port {at} of {len}");
            }
            Ok(I::at(at))
        })
        .collect()
}

impl RawReturned {
    fn decode(self, ports: &PortTable) -> wasmtime::Result<Returned> {
        let inputs = ports.inputs().len();
        Ok(Returned {
            outputs_changed: decode(self.outputs_changed, ports.outputs().len())?,
            inputs_changed: decode(self.inputs_changed, inputs)?,
            inputs_recorded: decode(self.inputs_recorded, inputs)?,
            inputs_cleared: decode(self.inputs_cleared, inputs)?,
            feedback_buffered: decode(self.feedback_buffered, ports.feedback().len())?,
        })
    }
}

/// Where a hub's functions are, in any instance of its composite.
pub(crate) struct HubExports {
    run: ComponentExportIndex,
    clear_input: ComponentExportIndex,
    latch_feedback: ComponentExportIndex,
    drop_feedback: ComponentExportIndex,
    get_output: IndexVec<OutputIdx, Option<ComponentExportIndex>>,
    set_output: IndexVec<OutputIdx, Option<ComponentExportIndex>>,
    read_handle: IndexVec<OutputIdx, Option<ComponentExportIndex>>,
    get_input: IndexVec<InputIdx, Option<ComponentExportIndex>>,
    set_input: IndexVec<InputIdx, Option<ComponentExportIndex>>,
    get_feedback: IndexVec<FeedbackIdx, Option<ComponentExportIndex>>,
    set_feedback: IndexVec<FeedbackIdx, Option<ComponentExportIndex>>,
}

impl HubExports {
    pub(crate) fn new(component: &Component, ports: &PortTable) -> wasmtime::Result<Self> {
        let interface = |name: &str| {
            component
                .get_export_index(None, name)
                .ok_or_else(|| wasmtime::format_err!("the composite exports no `{name}`"))
        };
        let control = interface(hub::CONTROL_INTERFACE)?;
        let port_funcs = interface(hub::PORTS_INTERFACE)?;
        let of_control = |name: &str| {
            component
                .get_export_index(Some(&control), name)
                .ok_or_else(|| wasmtime::format_err!("the hub's control has no `{name}`"))
        };
        let of_ports = |name: String| component.get_export_index(Some(&port_funcs), &name);
        Ok(Self {
            run: of_control("run")?,
            clear_input: of_control("clear-input")?,
            latch_feedback: of_control("latch-feedback")?,
            drop_feedback: of_control("drop-feedback")?,
            get_output: IndexVec::from_fn(ports.outputs().len(), |at| {
                of_ports(hub::get_output(at))
            }),
            set_output: IndexVec::from_fn(ports.outputs().len(), |at| {
                of_ports(hub::set_output(at))
            }),
            read_handle: IndexVec::from_fn(ports.outputs().len(), |at| {
                of_ports(hub::read_handle(at))
            }),
            get_input: IndexVec::from_fn(ports.inputs().len(), |at| of_ports(hub::get_input(at))),
            set_input: IndexVec::from_fn(ports.inputs().len(), |at| of_ports(hub::set_input(at))),
            get_feedback: IndexVec::from_fn(ports.feedback().len(), |at| {
                of_ports(hub::get_feedback(at))
            }),
            set_feedback: IndexVec::from_fn(ports.feedback().len(), |at| {
                of_ports(hub::set_feedback(at))
            }),
        })
    }
}

/// A live hub: an instance, with the functions the scheduler calls bound.
pub(crate) struct LiveHub {
    instance: Instance,
    exports: Arc<HubExports>,
    run: TypedFunc<(u32,), (RawReturned,)>,
    clear_input: TypedFunc<(u32,), (bool,)>,
    latch_feedback: TypedFunc<(u32,), (RawLatched,)>,
    drop_feedback: TypedFunc<(u32,), ()>,
}

impl HubExports {
    pub(crate) fn input_getter(&self, input: InputIdx) -> Option<ComponentExportIndex> {
        self.get_input[input]
    }

    pub(crate) fn output_getter(&self, output: OutputIdx) -> Option<ComponentExportIndex> {
        self.get_output[output]
    }
}

impl LiveHub {
    /// `T`, of a getter returning `option<T>`.
    pub(crate) fn getter_type<D: StoreData>(
        &self,
        store: &mut Store<D>,
        at: Option<ComponentExportIndex>,
    ) -> Option<Type> {
        let func = self.instance.get_func(store.as_context_mut(), at?)?;
        match func.ty(&*store).results().next()? {
            Type::Option(option) => Some(option.ty()),
            _ => None,
        }
    }

    fn new<D: StoreData>(
        store: &mut Store<D>,
        instance: Instance,
        exports: Arc<HubExports>,
    ) -> wasmtime::Result<Self> {
        let func = |store: &mut Store<D>, at: &ComponentExportIndex| {
            instance
                .get_func(store.as_context_mut(), *at)
                .ok_or_else(|| wasmtime::format_err!("the hub lacks a control function"))
        };
        let run = func(store, &exports.run)?.typed(&*store)?;
        let clear_input = func(store, &exports.clear_input)?.typed(&*store)?;
        let latch_feedback = func(store, &exports.latch_feedback)?.typed(&*store)?;
        let drop_feedback = func(store, &exports.drop_feedback)?.typed(&*store)?;
        Ok(Self {
            instance,
            exports,
            run,
            clear_input,
            latch_feedback,
            drop_feedback,
        })
    }

    /// The function at `at`, of a port that has one.
    pub(crate) fn func<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        at: Option<ComponentExportIndex>,
    ) -> wasmtime::Result<Func> {
        let at = at.ok_or_else(|| wasmtime::format_err!("the hub has no such port function"))?;
        acc.with(|mut access| self.instance.get_func(access.as_context_mut(), at))
            .ok_or_else(|| wasmtime::format_err!("the hub has no such port function"))
    }

    pub(crate) fn get_output<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        output: OutputIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.get_output[output])
    }

    pub(crate) fn set_output<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        output: OutputIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.set_output[output])
    }

    pub(crate) fn get_input<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        input: InputIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.get_input[input])
    }

    pub(crate) fn set_input<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        input: InputIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.set_input[input])
    }

    pub(crate) fn get_feedback<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        connection: FeedbackIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.get_feedback[connection])
    }

    pub(crate) fn set_feedback<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        connection: FeedbackIdx,
    ) -> wasmtime::Result<Func> {
        self.func(acc, self.exports.set_feedback[connection])
    }

    /// Clears an input. Returns whether it held a value.
    pub(crate) async fn clear_input<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        input: InputIdx,
    ) -> wasmtime::Result<bool> {
        let at = u32_of(input.position())?;
        Ok(self.clear_input.call_concurrent(acc, (at,)).await?.0)
    }

    /// The object a handle output holds, if it holds one: the hub lends it to
    /// the host, through the linker's `lend-handle`.
    pub(crate) async fn read_handle<D: StoreData>(
        &self,
        acc: &Accessor<D>,
        output: OutputIdx,
    ) -> wasmtime::Result<Option<Shared>> {
        let read = self.func(acc, self.exports.read_handle[output])?;
        let read = acc.with(|access| read.typed::<(), ()>(access))?;
        acc.with(|mut access| access.data_mut().host_state().lent = None);
        read.call_concurrent(acc, ()).await?;
        Ok(acc.with(|mut access| access.data_mut().host_state().lent.take()))
    }
}

/// A port number as a guest's.
fn u32_of(at: usize) -> wasmtime::Result<u32> {
    u32::try_from(at).map_err(|_| wasmtime::format_err!("port {at} is out of range"))
}

/// A future in the Store.
pub(crate) type LaneFuture<'a> = Pin<Box<dyn Future<Output = wasmtime::Result<()>> + Send + 'a>>;

/// A host's request of the live hub. It answers on a channel of its own. An `Err`
/// faults the graph: a guest trapped.
pub(crate) type Request<D> =
    Box<dyn for<'a> FnOnce(&'a Accessor<D>, &'a LiveHub) -> LaneFuture<'a> + Send>;

/// What a host writes to an input, kept to write again after a rebuild: the only
/// values kept outside the Store. A value of any type, as `dyn` lets an input
/// written through a typed port stay typed.
pub(crate) trait HostWrite<D: StoreData>: Send + Sync + 'static {
    /// Writes it. Returns whether it changed anything.
    fn write<'a>(
        &'a self,
        acc: &'a Accessor<D>,
        hub: &'a LiveHub,
        input: InputIdx,
    ) -> Pin<Box<dyn Future<Output = wasmtime::Result<bool>> + Send + 'a>>;

    /// As WAVE text, for a snapshot of a stopped graph. `None` if it has no
    /// text: a typed value is only ever lowered.
    fn wave(&self) -> Option<Result<String, String>>;

    /// The value, if it is a dynamic one: what tells, with no hub to ask, that a
    /// write to a stopped graph changes nothing.
    fn val(&self) -> Option<&Val>;
}

/// A dynamic value written to an input.
pub(crate) struct ValWrite(pub(crate) Val);

impl<D: StoreData> HostWrite<D> for ValWrite {
    fn write<'a>(
        &'a self,
        acc: &'a Accessor<D>,
        hub: &'a LiveHub,
        input: InputIdx,
    ) -> Pin<Box<dyn Future<Output = wasmtime::Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            let set = hub.set_input(acc, input)?;
            call_set(acc, set, self.0.clone()).await
        })
    }

    fn wave(&self) -> Option<Result<String, String>> {
        Some(self.0.to_wave().map_err(|e| e.to_string()))
    }

    fn val(&self) -> Option<&Val> {
        Some(&self.0)
    }
}

/// Calls a dynamic setter: it takes the value as a one-element list.
pub(crate) async fn call_set<D: StoreData>(
    acc: &Accessor<D>,
    set: Func,
    value: Val,
) -> wasmtime::Result<bool> {
    let mut results = [Val::Bool(false)];
    set.call_concurrent(acc, &[Val::List(vec![value])], &mut results)
        .await?;
    Ok(matches!(results[0], Val::Bool(true)))
}

/// Calls a dynamic getter: it returns `option<T>`.
pub(crate) async fn call_get<D: StoreData>(
    acc: &Accessor<D>,
    get: Func,
) -> wasmtime::Result<Option<Val>> {
    let mut results = [Val::Bool(false)];
    get.call_concurrent(acc, &[], &mut results).await?;
    match std::mem::replace(&mut results[0], Val::Bool(false)) {
        Val::Option(value) => Ok(value.map(|value| *value)),
        _ => wasmtime::bail!("a getter returns an option"),
    }
}

/// How to reach the live instance, while there is one.
pub(crate) struct Client<D: StoreData> {
    requests: UnboundedSender<Request<D>>,
    /// Where the instance reports.
    events: UnboundedSender<ControlEvent>,
    /// The server, for a waiting pull to drive, and to know it has ended.
    server: SharedFuture<BoxFuture<Outcome>>,
    /// Which instance it is: what a typed port's cached function belongs to.
    incarnation: witgraph_sched::Incarnation,
}

impl<D: StoreData> Clone for Client<D> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            events: self.events.clone(),
            server: self.server.clone(),
            incarnation: self.incarnation,
        }
    }
}

impl<D: StoreData> Client<D> {
    pub(crate) fn incarnation(&self) -> witgraph_sched::Incarnation {
        self.incarnation
    }

    /// What tells the scheduler what a request did, for a request whose asker may
    /// give up on it: the request still happens.
    pub(crate) fn reporter(&self) -> impl Fn(ControlKind) + Send + 'static {
        let (events, incarnation) = (self.events.clone(), self.incarnation);
        move |kind| {
            // The receiver lives as long as the scheduler; a send can only fail
            // while it is being dropped.
            let _ = events.unbounded_send(ControlEvent { incarnation, kind });
        }
    }

    /// Runs `body` in the Store, in the lane's order, driving the server until
    /// it is answered. A guest call failing in it faults the graph, which is
    /// [`RuntimeError::InstanceStopped`] here and the fault in `take_faults`.
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        body: impl for<'a> FnOnce(
            &'a Accessor<D>,
            &'a LiveHub,
        )
            -> Pin<Box<dyn Future<Output = wasmtime::Result<T>> + Send + 'a>>
        + Send
        + 'static,
    ) -> Result<T, RuntimeError> {
        let (reply, answer) = oneshot::channel();
        let request: Request<D> = Box::new(move |acc, hub| {
            Box::pin(async move {
                let answer = body(acc, hub).await?;
                // The asker may have given up.
                let _ = reply.send(answer);
                Ok(())
            })
        });
        self.requests
            .unbounded_send(request)
            .map_err(|_| RuntimeError::InstanceStopped)?;
        let server = self.server.clone();
        match futures::future::select(answer, server).await {
            Either::Left((Ok(answer), _)) => Ok(answer),
            Either::Left((Err(_), _)) | Either::Right(_) => Err(RuntimeError::InstanceStopped),
        }
    }
}

/// What a read needs of a [`HubPlane`], whatever its Store's data.
pub(crate) type ClientSlot<D> = Arc<Mutex<Option<Client<D>>>>;

/// `mutex`, whichever panic may have poisoned it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An instance of a composite, with its data set up.
pub(crate) struct Built<D: 'static> {
    pub(crate) store: Store<D>,
    pub(crate) hub: LiveHub,
}

/// Why an instance could not be built.
#[derive(Debug)]
pub(crate) struct BuildFailure {
    pub(crate) message: String,
    /// A `fatal` call made while instantiating (from a start function).
    pub(crate) fatal: Option<(NodeId, String)>,
}

/// Instantiates a composite in a fresh Store.
pub(crate) async fn build<D: StoreData>(
    engine: &Engine,
    pre: &InstancePre<D>,
    exports: &Arc<HubExports>,
    data: D,
    settings: StoreSettings,
) -> Result<Built<D>, BuildFailure> {
    let plain = |e: wasmtime::Error| BuildFailure {
        message: format!("{e:#}"),
        fatal: None,
    };
    let mut store = Store::new(engine, data);
    store.limiter(|data: &mut D| data.host_state() as &mut dyn wasmtime::ResourceLimiter);
    // Instantiation (a guest's `_initialize`, say) gets a budget of its own, as
    // big as a `run`'s: a spinning start function faults instead of hanging the
    // load. Every `run` resets the fuel again.
    store
        .fuel_async_yield_interval(Some(settings.yield_interval))
        .map_err(plain)?;
    store.set_fuel(settings.budget()).map_err(plain)?;
    store.set_hostcall_fuel(settings.hostcall_fuel);
    let instance = match pre.instantiate_async(&mut store).await {
        Ok(instance) => instance,
        Err(e) => {
            return Err(BuildFailure {
                message: format!("{e:#}"),
                fatal: store.data_mut().host_state().fatal.take(),
            });
        }
    };
    let hub = LiveHub::new(&mut store, instance, exports.clone()).map_err(plain)?;
    store.set_fuel(settings.budget()).map_err(plain)?;
    Ok(Built { store, hub })
}

/// The data of a new Store. It is the embedder's code: a panic in it fails this
/// Store (a rebuild then faults with `Restart`) rather than unwinding through the
/// scheduler while it swaps the graph's state.
pub(crate) fn store_data<H: Host>(
    host: &H,
    memory_limit: Option<usize>,
    shared_memory: &Arc<SharedMemory>,
    components: usize,
) -> wasmtime::Result<H::Data> {
    let state = HostState::new(memory_limit, shared_memory, components);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.store_data(state)))
        .unwrap_or_else(|_| Err(wasmtime::format_err!("`Host::store_data` panicked")))
}

/// A lane's work: what the scheduler asked, or what the host did.
enum Job<D: StoreData> {
    Latch(FeedbackIdx),
    Drop(FeedbackIdx),
    Request(Request<D>),
}

/// The inputs the host wrote, to write again in the next instance.
pub(crate) type Retained<D> = Arc<Mutex<IndexVec<InputIdx, Option<Arc<dyn HostWrite<D>>>>>>;

/// The inputs the host wrote, and what it wrote.
pub(crate) fn retained_writes<D: StoreData>(
    retained: &Retained<D>,
) -> Vec<(InputIdx, Arc<dyn HostWrite<D>>)> {
    lock(retained)
        .iter_enumerated()
        .filter_map(|(input, write)| Some((input, write.clone()?)))
        .collect()
}

/// What serving one instance needs.
struct ServerArgs<D: StoreData> {
    built: Result<Built<D>, ()>,
    build: Option<RebuildArgs<D>>,
    ports: Arc<PortTable>,
    retained: Retained<D>,
    settings: StoreSettings,
    serving: Serving,
    requests: UnboundedReceiver<Request<D>>,
}

/// What building a new instance needs.
struct RebuildArgs<D: 'static> {
    engine: Engine,
    pre: InstancePre<D>,
    exports: Arc<HubExports>,
    data: wasmtime::Result<D>,
}

/// Serves one instance, until it faults or the scheduler lets go of it.
async fn serve_hub<D: StoreData>(args: ServerArgs<D>) -> Outcome {
    let ServerArgs {
        built,
        build: rebuild,
        ports,
        retained,
        settings,
        serving,
        requests,
    } = args;
    let Serving {
        incarnation,
        commands,
        events,
        ..
    } = serving;
    let send = {
        let events = events.clone();
        move |kind| {
            // The receiver lives as long as the scheduler; a send can only fail
            // while it is being dropped.
            let _ = events.unbounded_send(ControlEvent { incarnation, kind });
        }
    };
    let rebuilt = rebuild.is_some();
    let Built { mut store, hub } = match (built, rebuild) {
        (Ok(built), _) => built,
        (Err(()), Some(rebuild)) => {
            let data = rebuild.data.map_err(|e| {
                let message = format!("store data: {e:#}");
                (NodeFault::Restart { message }, None)
            })?;
            build(
                &rebuild.engine,
                &rebuild.pre,
                &rebuild.exports,
                data,
                settings,
            )
            .await
            .map_err(|failure| match failure.fatal {
                Some((node, message)) => (NodeFault::Fatal { message }, Some(node)),
                None => {
                    let message = failure.message;
                    (NodeFault::Restart { message }, None)
                }
            })?
        }
        (Err(()), None) => {
            let message = "nothing to build the graph from".to_string();
            return Err((NodeFault::Restart { message }, None));
        }
    };
    let served = store
        .run_concurrent(async |acc| -> wasmtime::Result<()> {
            if rebuilt {
                for (input, write) in retained_writes(&retained) {
                    write.write(acc, &hub, input).await?;
                }
                send(ControlKind::Built);
            }
            serve_commands(acc, &hub, &ports, settings, commands, requests, &send).await
        })
        .await;
    match served.and_then(|inner| inner) {
        Ok(()) => Ok(()),
        Err(e) => {
            let fatal = store.data_mut().host_state().fatal.take();
            Err(classify(&e, fatal))
        }
    }
}

/// The server's loop: what is asked, as it is asked.
async fn serve_commands<D: StoreData>(
    acc: &Accessor<D>,
    hub: &LiveHub,
    ports: &PortTable,
    settings: StoreSettings,
    mut commands: UnboundedReceiver<Command>,
    mut requests: UnboundedReceiver<Request<D>>,
    send: &(impl Fn(ControlKind) + Send + Sync),
) -> wasmtime::Result<()> {
    type RunFuture<'a> =
        Pin<Box<dyn Future<Output = (NodeIdx, wasmtime::Result<(RawReturned,)>)> + Send + 'a>>;
    let mut runs: FuturesUnordered<RunFuture<'_>> = FuturesUnordered::new();
    let mut jobs: VecDeque<Job<D>> = VecDeque::new();
    let mut lane: Option<LaneFuture<'_>> = None;
    let mut quiet_at: Option<u64> = None;
    let budget = settings.budget();
    let run_fn = hub.run;
    let (latch_fn, drop_fn) = (hub.latch_feedback, hub.drop_feedback);
    let start_job = |job: Job<D>| -> LaneFuture<'_> {
        match job {
            Job::Latch(connection) => Box::pin(async move {
                let at = u32_of(connection.position())?;
                let (raw,) = latch_fn.call_concurrent(acc, (at,)).await?;
                let latched = match raw {
                    RawLatched::Unchanged => Latched::Unchanged,
                    RawLatched::Changed => Latched::Changed,
                    RawLatched::Cleared => Latched::Cleared,
                };
                send(ControlKind::Latched {
                    connection,
                    latched,
                });
                Ok(())
            }),
            Job::Drop(connection) => Box::pin(async move {
                let at = u32_of(connection.position())?;
                drop_fn.call_concurrent(acc, (at,)).await?;
                Ok(())
            }),
            Job::Request(request) => request(acc, hub),
        }
    };
    poll_fn(|cx| {
        loop {
            let mut progressed = false;
            loop {
                match commands.poll_next_unpin(cx) {
                    Poll::Ready(Some(Command::Run(node))) => {
                        let at = u32_of(node.position());
                        let fueled =
                            acc.with(|mut access| access.as_context_mut().set_fuel(budget));
                        runs.push(Box::pin(async move {
                            let called = async {
                                let at = at?;
                                fueled?;
                                run_fn.call_concurrent(acc, (at,)).await
                            };
                            (node, called.await)
                        }));
                        progressed = true;
                    }
                    Poll::Ready(Some(Command::LatchFeedback(connection))) => {
                        jobs.push_back(Job::Latch(connection));
                    }
                    Poll::Ready(Some(Command::DropFeedback(connection))) => {
                        jobs.push_back(Job::Drop(connection));
                    }
                    Poll::Ready(Some(Command::AwaitQuiet(at))) => quiet_at = Some(at),
                    // Whoever asked is gone, so nothing else matters.
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                    Poll::Pending => break,
                }
            }
            while let Poll::Ready(Some(request)) = requests.poll_next_unpin(cx) {
                jobs.push_back(Job::Request(request));
            }
            if lane.is_none()
                && let Some(job) = jobs.pop_front()
            {
                // With no guest at work, all that burns fuel is the hub doing what
                // the host asked, which no `run` should pay for. A guest at work
                // keeps its budget, so a host polling it cannot extend it.
                if runs.is_empty() && acc.poll_no_interesting_tasks(cx).is_ready() {
                    acc.with(|mut access| access.as_context_mut().set_fuel(budget))?;
                }
                lane = Some(start_job(job));
            }
            if let Some(job) = &mut lane
                && let Poll::Ready(done) = job.as_mut().poll(cx)
            {
                lane = None;
                progressed = true;
                if let Err(e) = done {
                    return Poll::Ready(Err(e));
                }
            }
            while let Poll::Ready(Some((node, result))) = runs.poll_next_unpin(cx) {
                let (raw,) = match result {
                    Ok(returned) => returned,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                let returned = match raw.decode(ports) {
                    Ok(returned) => returned,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                send(ControlKind::RunReturned { node, returned });
                progressed = true;
            }
            if let Some(at) = quiet_at
                && runs.is_empty()
                && lane.is_none()
                && jobs.is_empty()
                && acc.poll_no_interesting_tasks(cx).is_ready()
            {
                send(ControlKind::Quiet(at));
                quiet_at = None;
            }
            if !progressed {
                return Poll::Pending;
            }
        }
    })
    .await
}

/// What rebuilds the graph: all a [`HubPlane`] needs of its load.
pub(crate) struct Blueprint<H: Host> {
    pub(crate) host: H,
    pub(crate) engine: Engine,
    pub(crate) pre: InstancePre<H::Data>,
    pub(crate) exports: Arc<HubExports>,
    pub(crate) settings: StoreSettings,
    pub(crate) max_memory: Option<usize>,
    pub(crate) components: usize,
    pub(crate) shared_memory: Arc<SharedMemory>,
}

/// A composed graph's data plane.
pub(crate) struct HubPlane<H: Host> {
    pub(crate) blueprint: Blueprint<H>,
    pub(crate) ports: Arc<PortTable>,
    pub(crate) client: ClientSlot<H::Data>,
    pub(crate) retained: Retained<H::Data>,
    /// The types of the graph's Value ports.
    pub(crate) types: Arc<PortTypes>,
    /// The instance built at load, until its first serving.
    first: Mutex<Option<Built<H::Data>>>,
}

impl<H: Host> HubPlane<H> {
    pub(crate) fn new(
        blueprint: Blueprint<H>,
        ports: Arc<PortTable>,
        types: Arc<PortTypes>,
        first: Built<H::Data>,
    ) -> Self {
        let retained = IndexVec::from_fn(ports.inputs().len(), |_| None);
        Self {
            blueprint,
            ports,
            client: Arc::new(Mutex::new(None)),
            retained: Arc::new(Mutex::new(retained)),
            types,
            first: Mutex::new(Some(first)),
        }
    }
}

impl<H: Host> DataPlane for HubPlane<H> {
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome> {
        let incarnation = serving.incarnation;
        let blueprint = &self.blueprint;
        let (built, build) = match lock(&self.first).take() {
            Some(built) => (Ok(built), None),
            None => {
                let data = store_data(
                    &blueprint.host,
                    blueprint.max_memory,
                    &blueprint.shared_memory,
                    blueprint.components,
                );
                let rebuild = RebuildArgs {
                    engine: blueprint.engine.clone(),
                    pre: blueprint.pre.clone(),
                    exports: blueprint.exports.clone(),
                    data,
                };
                (Err(()), Some(rebuild))
            }
        };
        let (requests_tx, requests) = mpsc::unbounded();
        let events = serving.events.clone();
        let server: BoxFuture<Outcome> = boxed(serve_hub(ServerArgs {
            built,
            build,
            ports: self.ports.clone(),
            retained: self.retained.clone(),
            settings: blueprint.settings,
            serving,
            requests,
        }));
        let server = server.shared();
        *lock(&self.client) = Some(Client {
            requests: requests_tx,
            events,
            server: server.clone(),
            incarnation,
        });
        boxed(server)
    }

    fn stopped(&self) {
        // The last hold on the server, so the Store goes now.
        *lock(&self.client) = None;
    }
}

impl<H: Host> HubPorts for HubPlane<H> {
    type Type = Type;
    type Value = Val;
    type Write = Arc<dyn HostWrite<H::Data>>;

    // Everything goes through one lane, so a write queues behind what was asked.
    const WRITES_DIRECTLY: bool = false;

    fn input_type(&self, input: InputIdx) -> Option<&Type> {
        self.types.inputs[input].as_ref()
    }

    fn output_type(&self, output: OutputIdx) -> Option<&Type> {
        self.types.outputs[output].as_ref()
    }

    fn with_retained<R>(
        &self,
        f: impl FnOnce(&mut IndexVec<InputIdx, Option<Self::Write>>) -> R,
    ) -> R {
        f(&mut lock(&self.retained))
    }

    fn same_write(old: &Self::Write, new: &Self::Write) -> bool {
        // A typed value cannot be compared, so it counts as a change.
        old.val().is_some() && old.val() == new.val()
    }

    fn write_of(value: Val) -> Self::Write {
        Arc::new(ValWrite(value))
    }

    fn written_wave(write: &Self::Write) -> Option<Result<String, String>> {
        write.wave()
    }

    fn parse_value(ty: &Type, text: &str) -> Result<Val, String> {
        engine::parse_wave(ty, text)
    }

    fn value_wave(value: &Val) -> Result<String, String> {
        value.to_wave().map_err(|e| e.to_string())
    }

    fn is_up(&self) -> bool {
        lock(&self.client).is_some()
    }

    fn send_write(
        &self,
        input: InputIdx,
        write: Self::Write,
    ) -> BoxFuture<Result<(), RuntimeError>> {
        let client = lock(&self.client).clone();
        boxed(async move {
            let client = client.ok_or(RuntimeError::InstanceStopped)?;
            let report = client.reporter();
            client
                .call(move |acc, hub| {
                    Box::pin(async move {
                        let changed = write.write(acc, hub, input).await?;
                        report(ControlKind::HostWrote { input, changed });
                        Ok(())
                    })
                })
                .await
        })
    }

    fn send_clear(&self, input: InputIdx) -> BoxFuture<Result<(), RuntimeError>> {
        let client = lock(&self.client).clone();
        boxed(async move {
            let client = client.ok_or(RuntimeError::InstanceStopped)?;
            let report = client.reporter();
            client
                .call(move |acc, hub| {
                    Box::pin(async move {
                        let cleared = hub.clear_input(acc, input).await?;
                        report(ControlKind::HostCleared { input, cleared });
                        Ok(())
                    })
                })
                .await
        })
    }

    fn send_restore(&self, writes: Vec<(PortSlot, Val)>) -> BoxFuture<Result<(), RuntimeError>> {
        let client = lock(&self.client).clone();
        boxed(async move {
            let client = client.ok_or(RuntimeError::InstanceStopped)?;
            let report = client.reporter();
            client
                .call(move |acc, hub| {
                    Box::pin(async move {
                        for (slot, val) in writes {
                            let set = match slot {
                                PortSlot::Input(input) => hub.set_input(acc, input)?,
                                PortSlot::Output(output) => hub.set_output(acc, output)?,
                                PortSlot::Feedback(connection) => {
                                    hub.set_feedback(acc, connection)?
                                }
                            };
                            call_set(acc, set, val).await?;
                        }
                        report(ControlKind::Restored);
                        Ok(())
                    })
                })
                .await
        })
    }

    fn read(&self, slot: PortSlot) -> BoxFuture<Result<Option<Val>, String>> {
        let client = lock(&self.client).clone();
        boxed(async move {
            let client = client.ok_or_else(|| RuntimeError::InstanceStopped.to_string())?;
            client
                .call(move |acc, hub| {
                    Box::pin(async move {
                        let get = match slot {
                            PortSlot::Input(input) => hub.get_input(acc, input)?,
                            PortSlot::Output(output) => hub.get_output(acc, output)?,
                            PortSlot::Feedback(connection) => hub.get_feedback(acc, connection)?,
                        };
                        call_get(acc, get).await
                    })
                })
                .await
                .map_err(|e| e.to_string())
        })
    }
}
