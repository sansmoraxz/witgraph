//! The data plane on a JavaScript host: a composed graph's hub, as jco
//! transpiled it, reached through callbacks of the loader.
//!
//! As on wasmtime, the hub holds every value, and the host writes and reads one
//! port at a time when it asks. What differs is what a JavaScript host can see:
//! it cannot tell when guests have no work left, so a stream group counts as
//! drained as soon as its `run`s have returned. And `port` calls are plain calls:
//! jco serves a sync export while a `run` is parked.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use futures::FutureExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{Either, LocalBoxFuture, Shared as SharedFuture};
use futures::stream::{FuturesUnordered, StreamExt};
use js_sys::{Array, Function, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use witgraph_ir::wasm_wave::value::Type;
use witgraph_ir::{
    FeedbackIdx, Idx, IndexVec, InputIdx, NodeId, NodeIdx, OutputIdx, PortKind, PortTable,
};
use witgraph_sched::{
    BoxFuture, Command, ControlEvent, ControlKind, DataPlane, HubPorts, Incarnation, Latched,
    NodeFault, Outcome, PortSlot, Returned, ReturnedNumbers, RuntimeError, Serving, boxed,
    decode_returned,
};
use witgraph_wit::hub::{self, ControlFunc};

use crate::value::{Data, camel, describe, parse_wave};

/// What the loader provides.
pub(crate) struct Callbacks {
    /// `run(node) -> Promise<returned>`: the hub's `run` of a node, by its
    /// number, with the hooks around it.
    pub(crate) run: Function,
    /// `rebuild() -> Promise<void>`: a new instance of the composite, throwing or
    /// rejecting if it cannot be made (with an error marked `witgraphFatal` when a
    /// start function called `fatal`).
    pub(crate) rebuild: Function,
    /// `instance() -> { control, ports }`: the live instance's exports.
    pub(crate) instance: Function,
    /// `readHandle(name) -> object | undefined`: what the hub lends the host of a
    /// handle output, when its port function `name` is called.
    pub(crate) read_handle: Function,
    /// `abandon()`: the instance stopped, whatever was in flight in it: what it
    /// holds and what it still does belong to nothing now.
    pub(crate) abandon: Option<Function>,
}

/// The message of a thrown JavaScript value.
pub(crate) fn thrown(error: &JsValue) -> String {
    if error.is_instance_of::<js_sys::Error>()
        && let Some(message) = Reflect::get(error, &JsValue::from_str("message"))
            .ok()
            .and_then(|message| message.as_string())
    {
        #[cfg(feature = "addr2line")]
        let message = with_wasm_frames(error, message);
        return message;
    }
    describe(error)
}

/// `message`, then the WebAssembly frames of a trap's stack, one per line:
/// each function with its offset in the core module, which
/// `wasm-tools addr2line` takes. The JavaScript frames are left out: the
/// loader imports jco's output from a `data:` URL, and each of them names
/// all of it.
#[cfg(feature = "addr2line")]
fn with_wasm_frames(error: &JsValue, mut message: String) -> String {
    if !error.is_instance_of::<js_sys::WebAssembly::RuntimeError>() {
        return message;
    }
    let stack = Reflect::get(error, &JsValue::from_str("stack"))
        .ok()
        .and_then(|stack| stack.as_string())
        .unwrap_or_default();
    for frame in stack.lines().filter(|l| l.contains("wasm-function[")) {
        message.push('\n');
        message.push_str(frame.trim());
    }
    message
}

/// What a thrown JavaScript value is as a fault of `node`, the one whose call
/// threw: [`NodeFault::Fatal`] when it is marked `witgraphFatal`, naming the node
/// that called `fatal`.
fn fault_of(error: &JsValue, node: Option<NodeId>, otherwise: bool) -> (NodeFault, Option<NodeId>) {
    let message = thrown(error);
    let fatal =
        Reflect::get(error, &JsValue::from_str("witgraphFatal")).is_ok_and(|mark| mark.is_truthy());
    if fatal {
        let caller = Reflect::get(error, &JsValue::from_str("witgraphNode"))
            .ok()
            .and_then(|node| node.as_string())
            .map(NodeId::from);
        return (NodeFault::Fatal { message }, caller.or(node));
    }
    let fault = if otherwise {
        NodeFault::Restart { message }
    } else {
        NodeFault::WasmTrap { message }
    };
    (fault, node)
}

/// Awaits `value` if it is a promise.
async fn settle(value: JsValue) -> Result<JsValue, JsValue> {
    match value.dyn_into::<Promise>() {
        Ok(promise) => JsFuture::from(promise).await,
        Err(value) => Ok(value),
    }
}

/// Why a read of the hub failed.
pub(crate) enum ReadFailure {
    /// The hub threw: a failure of the instance.
    Thrown(String),
    /// What it returned is not what the port holds.
    Decode(String),
}

/// A list of port numbers of a `returned`, as numbers.
fn numbers(returned: &JsValue, field: &str) -> Result<Vec<u32>, String> {
    let list =
        Reflect::get(returned, &JsValue::from_str(field)).map_err(|_| format!("no `{field}`"))?;
    Array::from(&list)
        .iter()
        .map(|at| {
            at.as_f64()
                .filter(|at| at.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(at))
                .map(|at| at as u32)
                .ok_or_else(|| format!("`{field}` holds what is no port number"))
        })
        .collect()
}

/// What the hub's `run` returned, as the scheduler needs it.
fn returned_of(returned: &JsValue, ports: &PortTable) -> Result<Returned, String> {
    decode_returned(
        ports,
        ReturnedNumbers {
            outputs_changed: &numbers(returned, "outputsChanged")?,
            inputs_changed: &numbers(returned, "inputsChanged")?,
            inputs_recorded: &numbers(returned, "inputsRecorded")?,
            inputs_cleared: &numbers(returned, "inputsCleared")?,
            feedback_buffered: &numbers(returned, "feedbackBuffered")?,
        },
    )
}

/// A graph's hub, on this host.
pub(crate) struct WebHub {
    pub(crate) callbacks: Callbacks,
    pub(crate) ports: Rc<PortTable>,
    /// Payload type of every Value input port (inner type when optional).
    pub(crate) input_types: IndexVec<InputIdx, Option<Type>>,
    /// Type of every Value output port.
    pub(crate) output_types: IndexVec<OutputIdx, Option<Type>>,
    /// The inputs the host wrote, to write again in the next instance: the only
    /// values kept outside the hub.
    pub(crate) retained: RefCell<IndexVec<InputIdx, Option<Rc<Data>>>>,
    /// Whether an instance is up and built.
    pub(crate) live: Cell<bool>,
    /// Whether the next instance is the first, which the loader made.
    fresh: Cell<bool>,
    /// The server, and what says its instance is built.
    server: RefCell<Option<Server>>,
    /// Where a trap no call reports goes: one in a task a guest spawned, say.
    traps: RefCell<Option<UnboundedSender<JsValue>>>,
    /// Where the instance being served reports what a host call did.
    events: RefCell<Option<(UnboundedSender<ControlEvent>, Incarnation)>>,
}

/// The server, for a host waiting on a rebuild to drive.
#[derive(Clone)]
struct Server {
    future: SharedFuture<LocalBoxFuture<'static, Outcome>>,
    built: SharedFuture<oneshot::Receiver<()>>,
}

impl WebHub {
    pub(crate) fn new(
        callbacks: Callbacks,
        ports: Rc<PortTable>,
        input_types: IndexVec<InputIdx, Option<Type>>,
        output_types: IndexVec<OutputIdx, Option<Type>>,
    ) -> Rc<Self> {
        let retained = IndexVec::from_fn(ports.inputs().len(), |_| None);
        Rc::new(Self {
            callbacks,
            ports,
            input_types,
            output_types,
            retained: RefCell::new(retained),
            live: Cell::new(true),
            fresh: Cell::new(true),
            server: RefCell::new(None),
            traps: RefCell::new(None),
            events: RefCell::new(None),
        })
    }

    fn exports(&self, which: &str) -> Result<JsValue, String> {
        let instance = self
            .callbacks
            .instance
            .call0(&JsValue::NULL)
            .map_err(|e| thrown(&e))?;
        Reflect::get(&instance, &JsValue::from_str(which)).map_err(|e| thrown(&e))
    }

    /// Calls the hub's function `name` (a WIT name) of `ports` or `control`.
    fn call(&self, which: &str, name: &str, args: &[JsValue]) -> Result<JsValue, String> {
        let exports = self.exports(which)?;
        let function = Reflect::get(&exports, &JsValue::from_str(&camel(name)))
            .ok()
            .and_then(|f| f.dyn_into::<Function>().ok())
            .ok_or_else(|| format!("the hub has no `{name}`"))?;
        let args: Array = args.iter().collect();
        function.apply(&exports, &args).map_err(|e| thrown(&e))
    }

    /// A getter's `option<T>`, as a value of `T`.
    fn get(&self, name: &str, ty: &Type) -> Result<Option<Rc<Data>>, ReadFailure> {
        let js = self.call("ports", name, &[]).map_err(ReadFailure::Thrown)?;
        crate::value::option_payload_js(&js, ty)
            .map(|payload| Data::from_output(payload, ty.clone()).map(Rc::new))
            .transpose()
            .map_err(ReadFailure::Decode)
    }

    /// What the host is told of a read: as on wasmtime, a hub failing in it faults
    /// the graph, which then holds nothing to read.
    pub(crate) fn host_read<T>(
        &self,
        read: Result<Option<T>, ReadFailure>,
    ) -> Result<Option<T>, String> {
        match read {
            Ok(value) => Ok(value),
            Err(ReadFailure::Thrown(message)) => {
                self.fault(message);
                Ok(None)
            }
            Err(ReadFailure::Decode(message)) => Err(message),
        }
    }

    /// A setter: it takes the value as a one-element list.
    fn set(&self, name: &str, data: &Data) -> Result<bool, String> {
        let list: Array = std::iter::once(data.js().clone()).collect();
        let changed = self.call("ports", name, &[list.into()])?;
        Ok(changed.is_truthy())
    }

    pub(crate) fn get_output(&self, output: OutputIdx) -> Result<Option<Rc<Data>>, ReadFailure> {
        let ty = self.output_types[output]
            .as_ref()
            .ok_or_else(not_a_value_port)?;
        self.get(&hub::get_output(output), ty)
    }

    pub(crate) fn get_input(&self, input: InputIdx) -> Result<Option<Rc<Data>>, ReadFailure> {
        let ty = self.input_types[input]
            .as_ref()
            .ok_or_else(not_a_value_port)?;
        self.get(&hub::get_input(input), ty)
    }

    pub(crate) fn get_feedback(
        &self,
        connection: FeedbackIdx,
    ) -> Result<Option<Rc<Data>>, ReadFailure> {
        let from = self.ports.feedback()[connection].from;
        let ty = self.output_types[from]
            .as_ref()
            .ok_or_else(not_a_value_port)?;
        self.get(&hub::get_feedback(connection), ty)
    }

    pub(crate) fn set_input(&self, input: InputIdx, data: &Data) -> Result<bool, String> {
        self.set(&hub::set_input(input), data)
    }

    pub(crate) fn set_output(&self, output: OutputIdx, data: &Data) -> Result<bool, String> {
        self.set(&hub::set_output(output), data)
    }

    pub(crate) fn set_feedback(
        &self,
        connection: FeedbackIdx,
        data: &Data,
    ) -> Result<bool, String> {
        self.set(&hub::set_feedback(connection), data)
    }

    pub(crate) fn clear_input(&self, input: InputIdx) -> Result<bool, String> {
        let number = JsValue::from(input.position() as u32);
        Ok(self
            .call("control", ControlFunc::ClearInput.name(), &[number])?
            .is_truthy())
    }

    pub(crate) fn read_handle(&self, output: OutputIdx) -> Result<Option<JsValue>, ReadFailure> {
        let name = JsValue::from_str(&camel(&hub::read_handle(output)));
        let handle = self
            .callbacks
            .read_handle
            .call1(&JsValue::NULL, &name)
            .map_err(|e| ReadFailure::Thrown(thrown(&e)))?;
        Ok((!handle.is_undefined()).then_some(handle))
    }

    /// Tells the scheduler what a host call did.
    fn report(&self, kind: ControlKind) {
        if let Some((events, incarnation)) = &*self.events.borrow() {
            // The receiver lives as long as the scheduler; a send can only fail
            // while it is being dropped.
            let _ = events.unbounded_send(ControlEvent {
                incarnation: *incarnation,
                kind,
            });
        }
    }

    /// A hub failing as the host calls it is a fault, as a trap in a task is.
    fn fault(&self, message: String) {
        self.report_trap(JsValue::from_str(&message));
    }

    /// Reports a trap of the live instance, whichever task it was in.
    pub(crate) fn report_trap(&self, error: JsValue) {
        if let Some(traps) = &*self.traps.borrow() {
            let _ = traps.unbounded_send(error);
        }
    }

    /// Waits for the instance being served to be built, driving it. An instance
    /// that cannot be, or none, is [`RuntimeError::InstanceStopped`].
    pub(crate) async fn built(&self) -> Result<(), RuntimeError> {
        let server = self.server.borrow().clone();
        let Some(server) = server else {
            return Err(RuntimeError::InstanceStopped);
        };
        match futures::future::select(server.built, server.future).await {
            Either::Left((Ok(()), _)) => Ok(()),
            Either::Left((Err(_), _)) | Either::Right(_) => Err(RuntimeError::InstanceStopped),
        }
    }
}

fn not_a_value_port() -> ReadFailure {
    ReadFailure::Decode("not a Value port".into())
}

/// Per node, the stream and future inputs its outputs feed, for the loader to
/// watch on their way.
pub(crate) fn stream_inputs(
    ports: &PortTable,
    compiled: &witgraph_ir::CompiledGraph,
) -> HashMap<NodeId, Vec<(String, PortKind, InputIdx)>> {
    let mut edges: HashMap<NodeId, Vec<(String, PortKind, InputIdx)>> = HashMap::new();
    for resolved in compiled.connections() {
        let conn = resolved.connection;
        if !resolved.from.kind().is_async() {
            continue;
        }
        if let Some(input) = ports.input(&conn.to.node, &conn.to.port) {
            edges.entry(conn.from.node.clone()).or_default().push((
                conn.from.port.to_string(),
                resolved.from.kind(),
                input,
            ));
        }
    }
    edges
}

/// What a call of a node's `run` ended with.
type CallDone = (NodeIdx, Result<JsValue, JsValue>);

/// Serves one instance.
async fn serve(
    hub: Rc<WebHub>,
    fresh: bool,
    built: oneshot::Sender<()>,
    mut traps: UnboundedReceiver<JsValue>,
    serving: Serving,
) -> Outcome {
    let Serving {
        incarnation,
        mut commands,
        events,
        ..
    } = serving;
    let send = |kind| {
        // The receiver lives as long as the scheduler; a send can only fail
        // while it is being dropped.
        let _ = events.unbounded_send(ControlEvent { incarnation, kind });
    };
    if !fresh {
        let rebuilt = match hub.callbacks.rebuild.call0(&JsValue::NULL) {
            Ok(result) => settle(result).await,
            Err(error) => Err(error),
        };
        rebuilt.map_err(|error| fault_of(&error, None, true))?;
        hub.live.set(true);
        let retained: Vec<(InputIdx, Rc<Data>)> = hub
            .retained
            .borrow()
            .iter_enumerated()
            .filter_map(|(input, data)| Some((input, data.clone()?)))
            .collect();
        for (input, data) in retained {
            hub.set_input(input, &data)
                .map_err(|message| (NodeFault::WasmTrap { message }, None))?;
        }
        send(ControlKind::Built);
    }
    let _ = built.send(());
    let trap = |message: String| (NodeFault::WasmTrap { message }, None);
    let mut calls = FuturesUnordered::new();
    let mut closed = false;
    loop {
        futures::select_biased! {
            done = calls.select_next_some() => {
                let (node, result): CallDone = done;
                let id = hub.ports.node_id(node).clone();
                let returned = result.map_err(|error| fault_of(&error, Some(id), false))?;
                let returned = returned_of(&returned, &hub.ports).map_err(trap)?;
                send(ControlKind::RunReturned { node, returned });
            }
            error = traps.select_next_some() => {
                return Err(fault_of(&error, None, false));
            }
            command = commands.next() => match command {
                Some(Command::Run(node)) => {
                    let number = JsValue::from(node.position() as u32);
                    let called = hub.callbacks.run.call1(&JsValue::NULL, &number);
                    calls.push(async move {
                        let result = match called {
                            Ok(result) => settle(result).await,
                            Err(error) => Err(error),
                        };
                        (node, result)
                    });
                }
                Some(Command::LatchFeedback(connection)) => {
                    let number = JsValue::from(connection.position() as u32);
                    let latched = hub
                        .call("control", ControlFunc::LatchFeedback.name(), &[number])
                        .map_err(trap)?;
                    let latched = match latched.as_string().as_deref() {
                        Some("unchanged") => Latched::Unchanged,
                        Some("changed") => Latched::Changed,
                        Some("cleared") => Latched::Cleared,
                        other => {
                            return Err(trap(format!(
                                "`latch-feedback` returned {other:?}, which is no `latched`"
                            )));
                        }
                    };
                    send(ControlKind::Latched { connection, latched });
                }
                Some(Command::DropFeedback(connection)) => {
                    let number = JsValue::from(connection.position() as u32);
                    hub.call("control", ControlFunc::DropFeedback.name(), &[number]).map_err(trap)?;
                }
                Some(Command::AwaitQuiet(at)) => send(ControlKind::Quiet(at)),
                None => closed = true,
            },
            complete => break,
        }
        if closed && calls.is_empty() {
            break;
        }
    }
    Ok(())
}

/// The data plane, as the scheduler sees it.
pub(crate) struct WebPlane(pub(crate) Rc<WebHub>);

impl DataPlane for WebPlane {
    fn serve(&self, serving: Serving) -> BoxFuture<Outcome> {
        let hub = self.0.clone();
        let fresh = hub.fresh.replace(false);
        hub.live.set(fresh);
        let (built_tx, built) = oneshot::channel();
        let (traps_tx, traps) = mpsc::unbounded();
        *hub.traps.borrow_mut() = Some(traps_tx);
        *hub.events.borrow_mut() = Some((serving.events.clone(), serving.incarnation));
        let future: LocalBoxFuture<'static, Outcome> =
            serve(hub.clone(), fresh, built_tx, traps, serving).boxed_local();
        let future = future.shared();
        *hub.server.borrow_mut() = Some(Server {
            future: future.clone(),
            built: built.shared(),
        });
        boxed(future)
    }

    fn stopped(&self) {
        self.0.live.set(false);
        *self.0.traps.borrow_mut() = None;
        *self.0.events.borrow_mut() = None;
        // The last hold on the server, so calls in flight are abandoned now.
        *self.0.server.borrow_mut() = None;
        if let Some(abandon) = &self.0.callbacks.abandon {
            let _ = abandon.call0(&JsValue::NULL);
        }
    }
}

impl HubPorts for WebPlane {
    type Type = Type;
    type Value = Rc<Data>;
    type Write = Rc<Data>;

    // `port` calls are plain calls, which do not wait for what the server has
    // yet to serve.
    const WRITES_DIRECTLY: bool = true;

    fn input_type(&self, input: InputIdx) -> Option<&Type> {
        self.0.input_types[input].as_ref()
    }

    fn output_type(&self, output: OutputIdx) -> Option<&Type> {
        self.0.output_types[output].as_ref()
    }

    fn with_retained<R>(
        &self,
        f: impl FnOnce(&mut IndexVec<InputIdx, Option<Rc<Data>>>) -> R,
    ) -> R {
        f(&mut self.0.retained.borrow_mut())
    }

    fn same_write(old: &Rc<Data>, new: &Rc<Data>) -> bool {
        old.equals(new)
    }

    fn write_of(value: Rc<Data>) -> Rc<Data> {
        value
    }

    fn written_wave(write: &Rc<Data>) -> Option<Result<String, String>> {
        Some(write.wave())
    }

    fn parse_value(ty: &Type, text: &str) -> Result<Rc<Data>, String> {
        let value = parse_wave(ty, text)?;
        Ok(Rc::new(Data::from_value(value, ty.clone())))
    }

    fn value_wave(value: &Rc<Data>) -> Result<String, String> {
        value.wave()
    }

    fn is_up(&self) -> bool {
        self.0.live.get()
    }

    fn send_write(&self, input: InputIdx, write: Rc<Data>) -> BoxFuture<Result<(), RuntimeError>> {
        let hub = self.0.clone();
        boxed(async move {
            match hub.set_input(input, &write) {
                Ok(changed) => hub.report(ControlKind::HostWrote { input, changed }),
                Err(message) => hub.fault(message),
            }
            Ok(())
        })
    }

    fn send_clear(&self, input: InputIdx) -> BoxFuture<Result<(), RuntimeError>> {
        let hub = self.0.clone();
        boxed(async move {
            match hub.clear_input(input) {
                Ok(cleared) => hub.report(ControlKind::HostCleared { input, cleared }),
                Err(message) => hub.fault(message),
            }
            Ok(())
        })
    }

    fn send_restore(
        &self,
        writes: Vec<(PortSlot, Rc<Data>)>,
    ) -> BoxFuture<Result<(), RuntimeError>> {
        let hub = self.0.clone();
        boxed(async move {
            // An instance that cannot be built is a fault for the next tick to report.
            hub.built().await?;
            let written = writes.iter().try_for_each(|(slot, val)| {
                match slot {
                    PortSlot::Input(input) => hub.set_input(*input, val),
                    PortSlot::Output(output) => hub.set_output(*output, val),
                    PortSlot::Feedback(connection) => hub.set_feedback(*connection, val),
                }
                .map(|_| ())
            });
            match written {
                Ok(()) => hub.report(ControlKind::Restored),
                Err(message) => hub.fault(message),
            }
            Ok(())
        })
    }

    fn read(&self, slot: PortSlot) -> BoxFuture<Result<Option<Rc<Data>>, String>> {
        let hub = self.0.clone();
        boxed(async move {
            let read = match slot {
                PortSlot::Input(input) => hub.get_input(input),
                PortSlot::Output(output) => hub.get_output(output),
                PortSlot::Feedback(connection) => hub.get_feedback(connection),
            };
            read.map_err(|failure| match failure {
                ReadFailure::Thrown(message) => {
                    hub.fault(message.clone());
                    message
                }
                ReadFailure::Decode(message) => message,
            })
        })
    }
}
