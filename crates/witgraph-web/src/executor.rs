//! The JavaScript executor: nodes are functions of the embedding page.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect};
use std::sync::Arc;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use witgraph_ir::wasm_wave::value::Type;
use witgraph_ir::{NodeId, PortDirection, PortKind, PortRef};
use witgraph_sched::{
    BoxFuture, CallFuture, Canonical, Executor, MemberPlan, NodeFault, OptionPayload, Outcome,
    boxed,
};

use crate::value::{
    Data, Resource, WebValue, check_type, describe, none_to_js, parse_wave, set_field, some_js,
    with_key,
};

/// One output port of a node: its kind and, for a Value port, its type.
pub(crate) struct OutputPort {
    pub(crate) name: String,
    pub(crate) kind: PortKind,
    pub(crate) ty: Option<Type>,
}

/// What the page provides: how to run and rebuild a node, and optionally
/// how to close a stream nothing reads and drop abandoned runs.
pub(crate) struct Callbacks {
    /// `run(node, inputs) -> outputs | Promise<outputs>`.
    pub(crate) run: Function,
    /// `rebuild(node) -> void | Promise<void>`: gives the node fresh guest
    /// state.
    pub(crate) rebuild: Function,
    /// `close(value)`: drops a stream or future output nothing reads.
    pub(crate) close: Option<Function>,
    /// `abandon(nodes)`: the instance of those nodes stopped before its calls
    /// did (a shutdown, a restore, a fault): their `rebuild` and `run` calls
    /// still in flight belong to nothing now.
    pub(crate) abandon: Option<Function>,
}

/// What every call shares.
pub(crate) struct Shared {
    pub(crate) callbacks: Callbacks,
    /// Payload type of every Value input port (inner type when optional).
    pub(crate) input_types: HashMap<PortRef, Type>,
    /// Type of every Value output port.
    pub(crate) output_types: HashMap<PortRef, Type>,
    /// Every node's output ports.
    pub(crate) outputs: HashMap<NodeId, Vec<OutputPort>>,
    /// Every node, in the graph's topological order.
    pub(crate) nodes: Vec<NodeId>,
}

/// The executor: runs nodes through the page's callbacks. The page owns the node
/// instances, so nothing is kept on this side between calls.
pub(crate) struct Js {
    pub(crate) shared: Rc<Shared>,
}

/// The message of a thrown JavaScript value.
fn thrown(error: &JsValue) -> String {
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

/// Whether a thrown JavaScript value is the page's `fatal`: an error
/// marked `witgraphFatal`.
fn is_fatal(error: &JsValue) -> bool {
    Reflect::get(error, &JsValue::from_str("witgraphFatal")).is_ok_and(|mark| mark.is_truthy())
}

/// Awaits `value` if it is a promise.
async fn settle(value: JsValue) -> Result<JsValue, JsValue> {
    match value.dyn_into::<Promise>() {
        Ok(promise) => JsFuture::from(promise).await,
        Err(value) => Ok(value),
    }
}

/// Calls a node's `run`, with one value per field of its `inputs` record.
async fn call(
    shared: Rc<Shared>,
    plan: Arc<MemberPlan>,
    args: Vec<Option<WebValue>>,
) -> Result<Vec<(String, WebValue)>, (NodeFault, Option<NodeId>)> {
    let node = &plan.node;
    // Each node is an instance of its own: what goes wrong in a call is that
    // node's.
    let fail = |message: String| (NodeFault::WasmTrap { message }, Some(node.clone()));
    let inputs = Object::new();
    let mut args = args.into_iter();
    for field in plan.inputs.iter().flatten() {
        // An optional field is the option; its value the payload.
        let ty = shared.input_types.get(&field.port);
        let js = match args.next().flatten() {
            Some(WebValue::Handle(js)) => js,
            // Lent: jco lowers it as the `borrow` the input is.
            Some(WebValue::Resource(resource)) => resource.js().clone(),
            // Passed on as it is: a guest's output converts nothing.
            Some(WebValue::Data(data)) => match ty {
                Some(ty) if field.optional => some_js(&data, ty),
                Some(_) => data.js().clone(),
                None => JsValue::UNDEFINED,
            },
            // The data plane has checked that every required input has one.
            None if field.optional => ty.map_or(JsValue::UNDEFINED, none_to_js),
            None => return Err(fail(format!("`{node}.{}` has no value", field.name))),
        };
        set_field(&inputs, field.name.as_str(), &js);
    }
    let called =
        shared
            .callbacks
            .run
            .call2(&JsValue::NULL, &JsValue::from_str(node.as_str()), &inputs);
    let result = match called {
        Ok(result) => settle(result).await,
        Err(error) => Err(error),
    };
    let result = result.map_err(|error| {
        let message = thrown(&error);
        let fault = if is_fatal(&error) {
            NodeFault::Fatal { message }
        } else {
            NodeFault::WasmTrap { message }
        };
        (fault, Some(node.clone()))
    })?;
    let mut outputs = Vec::new();
    for port in shared.outputs.get(node).into_iter().flatten() {
        let js =
            with_key(&port.name, |key| Reflect::get(&result, key)).unwrap_or(JsValue::UNDEFINED);
        let value = match (port.kind, &port.ty) {
            (PortKind::Value, Some(ty)) => {
                WebValue::Data(Rc::new(Data::from_output(js, ty.clone()).map_err(|e| {
                    fail(format!("`{node}` returned a bad `{}`: {e}", port.name))
                })?))
            }
            (PortKind::Handle, _) if js.is_object() => {
                WebValue::Resource(Rc::new(Resource::new(js)))
            }
            (PortKind::Handle, _) => {
                return Err(fail(format!(
                    "`{node}` returned a bad `{}`: not a resource",
                    port.name
                )));
            }
            _ => WebValue::Handle(js),
        };
        outputs.push((port.name.clone(), value));
    }
    Ok(outputs)
}

/// Gives every node fresh guest state through the page's `rebuild`, all at once.
/// Once every rebuild has settled, the first node whose rebuild failed is the
/// culprit: its fault is [`NodeFault::Fatal`] when its error is marked
/// `witgraphFatal` (a start function called `fatal`), else
/// [`NodeFault::Restart`].
async fn rebuild_nodes(shared: Rc<Shared>) -> Outcome {
    let rebuilds = shared.nodes.iter().map(|node| {
        let called = shared
            .callbacks
            .rebuild
            .call1(&JsValue::NULL, &JsValue::from_str(node.as_str()));
        async move {
            match called {
                Ok(result) => settle(result).await.map(|_| ()),
                Err(error) => Err(error),
            }
        }
    });
    let rebuilt = futures::future::join_all(rebuilds).await;
    for (node, result) in shared.nodes.iter().zip(rebuilt) {
        if let Err(error) = result {
            let message = thrown(&error);
            let fault = if is_fatal(&error) {
                NodeFault::Fatal { message }
            } else {
                NodeFault::Restart {
                    message: format!("`{node}`: {message}"),
                }
            };
            return Err((fault, Some(node.clone())));
        }
    }
    Ok(())
}

impl Executor for Js {
    type Value = WebValue;
    type Type = Type;

    fn call(&self, member: Arc<MemberPlan>, args: Vec<Option<WebValue>>) -> CallFuture<WebValue> {
        boxed(call(self.shared.clone(), member, args))
    }

    fn rebuild(&self) -> BoxFuture<Outcome> {
        boxed(rebuild_nodes(self.shared.clone()))
    }

    fn close(&self, value: WebValue) {
        if let (WebValue::Handle(js), Some(close)) = (value, &self.shared.callbacks.close) {
            // Closing is best effort: nothing reads the value either way.
            let _ = close.call1(&JsValue::NULL, &js);
        }
    }

    fn abandon(&self, nodes: &[NodeId]) {
        if let Some(abandon) = &self.shared.callbacks.abandon {
            let nodes: Array = nodes
                .iter()
                .map(|node| JsValue::from_str(node.as_str()))
                .collect();
            let _ = abandon.call1(&JsValue::NULL, &nodes);
        }
    }

    fn option_payload(value: &WebValue) -> OptionPayload<'_, WebValue> {
        match value {
            WebValue::Data(data) => match data.option_payload() {
                Ok(Some(payload)) => {
                    OptionPayload::Some(Cow::Owned(WebValue::Data(Rc::new(payload))))
                }
                Ok(None) => OptionPayload::None,
                Err(()) => OptionPayload::NotOption,
            },
            WebValue::Handle(_) | WebValue::Resource(_) => OptionPayload::NotOption,
        }
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&Type> {
        match direction {
            PortDirection::Input => self.shared.input_types.get(port),
            PortDirection::Output => self.shared.output_types.get(port),
        }
    }

    fn check_input(&self, ty: &Type, value: WebValue) -> Result<Canonical<WebValue>, String> {
        let WebValue::Data(data) = value else {
            return Err("a stream, future or handle is not a Value".into());
        };
        check_type(data.value()?, ty)?;
        Ok(Canonical::assume(WebValue::Data(data)))
    }

    fn parse_wave(&self, ty: &Type, text: &str) -> Result<Canonical<WebValue>, String> {
        parse_wave(ty, text).map(|value| {
            Canonical::assume(WebValue::Data(Rc::new(Data::from_value(value, ty.clone()))))
        })
    }

    fn to_wave(&self, value: &WebValue) -> Result<String, String> {
        match value {
            WebValue::Data(data) => data.wave(),
            WebValue::Handle(_) => Err("a stream or future is not a Value".into()),
            WebValue::Resource(_) => Err("a handle has no WAVE text".into()),
        }
    }
}
