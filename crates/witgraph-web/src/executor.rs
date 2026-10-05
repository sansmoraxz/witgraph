//! The JavaScript executor: nodes are functions of the embedding page.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use futures::FutureExt;
use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use witgraph_ir::wasm_wave::value::Type;
use witgraph_ir::wasm_wave::wasm::{WasmTypeKind, WasmValue};
use witgraph_ir::{NodeId, PortDirection, PortKind, PortName, PortRef};
use witgraph_sched::plan::IslandPlan;
use witgraph_sched::{
    Executor, Generation, GenerationFuture, IslandEventKind, NodeCaller, NodeFault, OptionPayload,
    drive, missing_input_message,
};

use crate::value::{
    WebValue, check_type, describe, from_js, none_to_js, parse_wave, set_field, some_to_js, to_js,
    to_wave, with_key,
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
    /// `abandon(nodes)`: the generation of those nodes' island ended
    /// before its calls did (a cancel, a shutdown, a restore, a sibling's
    /// failed `run`): their `rebuild` and `run` calls still in flight
    /// belong to nothing now.
    pub(crate) abandon: Option<Function>,
}

/// Tells the page, if a generation ends before its calls do, that those
/// still in flight are abandoned.
struct Abandon<'a> {
    shared: &'a Shared,
    plan: &'a IslandPlan,
    finished: bool,
}

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(abandon) = &self.shared.callbacks.abandon {
            let nodes: Array = self
                .plan
                .members
                .iter()
                .map(|m| JsValue::from_str(m.node.as_str()))
                .collect();
            let _ = abandon.call1(&JsValue::NULL, &nodes);
        }
    }
}

/// What every generation shares.
pub(crate) struct Shared {
    pub(crate) callbacks: Callbacks,
    /// Payload type of every Value input port (inner type when optional).
    pub(crate) input_types: HashMap<PortRef, Type>,
    /// Type of every Value output port.
    pub(crate) output_types: HashMap<PortRef, Type>,
    /// Every node's output ports.
    pub(crate) outputs: HashMap<NodeId, Vec<OutputPort>>,
    /// Every Value output's latest value: the scheduler's latched outputs,
    /// then each value a `run` returns. What `readOutput` reports while a
    /// tick holds the scheduler.
    pub(crate) latest: RefCell<HashMap<PortRef, WebValue>>,
    /// The outputs a `run` returned since `latest` was last made exact.
    pub(crate) touched: RefCell<HashSet<PortRef>>,
}

/// The executor: runs nodes through the page's callbacks. An island keeps
/// no state on this side between generations (the page owns the node
/// instances), so its live island is `()`.
pub(crate) struct Js {
    pub(crate) shared: Rc<Shared>,
}

/// What went wrong in a call: the message, and whether the node called
/// `fatal`.
pub(crate) struct CallError {
    message: String,
    fatal: bool,
    node: Option<NodeId>,
}

/// The message of a thrown JavaScript value.
fn thrown(error: &JsValue) -> String {
    if error.is_instance_of::<js_sys::Error>()
        && let Some(message) = Reflect::get(error, &JsValue::from_str("message"))
            .ok()
            .and_then(|message| message.as_string())
    {
        return message;
    }
    describe(error)
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

struct Members<'a> {
    shared: &'a Shared,
    plan: &'a IslandPlan,
}

impl NodeCaller<WebValue> for Members<'_> {
    type Error = CallError;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<WebValue>>,
    ) -> Result<Vec<(String, WebValue)>, CallError> {
        let plan = &self.plan.members[member];
        let node = &plan.node;
        // Each node is an instance of its own: what goes wrong in a call
        // is that node's.
        let fail = |message: String| CallError {
            message,
            fatal: false,
            node: Some(node.clone()),
        };
        let inputs = Object::new();
        let mut args = args.into_iter();
        for field in plan.inputs.iter().flatten() {
            // An optional field is the option; its value the payload.
            let ty = self.shared.input_types.get(&field.port);
            let js = match args.next().flatten() {
                Some(WebValue::Handle(js)) => js,
                Some(WebValue::Data(value)) => match ty {
                    Some(ty) if field.optional => some_to_js(&value, ty),
                    Some(ty) => to_js(&value, ty),
                    None => JsValue::UNDEFINED,
                },
                // `drive` has checked that every required input has one.
                None if field.optional => ty.map_or(JsValue::UNDEFINED, none_to_js),
                None => return Err(fail(format!("`{node}.{}` has no value", field.name))),
            };
            set_field(&inputs, field.name.as_str(), &js);
        }
        let called = self.shared.callbacks.run.call2(
            &JsValue::NULL,
            &JsValue::from_str(node.as_str()),
            &inputs,
        );
        let result = match called {
            Ok(result) => settle(result).await,
            Err(error) => Err(error),
        };
        let result = result.map_err(|error| CallError {
            message: thrown(&error),
            fatal: is_fatal(&error),
            node: Some(node.clone()),
        })?;
        let mut outputs = Vec::new();
        for port in self.shared.outputs.get(node).into_iter().flatten() {
            let js = with_key(&port.name, |key| Reflect::get(&result, key))
                .unwrap_or(JsValue::UNDEFINED);
            let value = match (port.kind, &port.ty) {
                (PortKind::Value, Some(ty)) => {
                    WebValue::Data(Rc::new(from_js(&js, ty).map_err(|e| {
                        fail(format!("`{node}` returned a bad `{}`: {e}", port.name))
                    })?))
                }
                _ => WebValue::Handle(js),
            };
            outputs.push((port.name.clone(), value));
        }
        let mut latest = self.shared.latest.borrow_mut();
        let mut touched = self.shared.touched.borrow_mut();
        for (port, value) in &outputs {
            if let WebValue::Data(_) = value {
                let port = PortRef::new(node.clone(), port.as_str());
                latest.insert(port.clone(), value.clone());
                touched.insert(port);
            }
        }
        Ok(outputs)
    }

    fn missing_input(&self, member: usize, field: &PortName) -> CallError {
        let node = self.plan.members.get(member).map(|m| m.node.clone());
        CallError {
            message: missing_input_message(self.plan, member, field),
            fatal: false,
            node,
        }
    }

    fn close(&self, value: WebValue) -> Result<(), CallError> {
        if let (WebValue::Handle(js), Some(close)) = (value, &self.shared.callbacks.close) {
            // Closing is best effort: nothing reads the value either way.
            let _ = close.call1(&JsValue::NULL, &js);
        }
        Ok(())
    }
}

/// Runs one generation: rebuilds its members first when `rebuild` is set,
/// then calls every member's `run`, in dependency order.
async fn run(
    shared: Rc<Shared>,
    mut generation: Generation<WebValue>,
    rebuild: bool,
) -> Result<(), (NodeFault, Option<NodeId>)> {
    let external = std::mem::take(&mut generation.external);
    let mut abandon = Abandon {
        shared: &shared,
        plan: &generation.plan,
        finished: false,
    };
    if rebuild {
        rebuild_members(&shared, &generation.plan)
            .await
            .inspect_err(|_| {
                // Every rebuild has settled: nothing of this generation is
                // left in flight.
                abandon.finished = true;
            })?;
        generation.send(IslandEventKind::Rebuilt);
    }
    let members = Members {
        shared: &shared,
        plan: &generation.plan,
    };
    let send = |kind| generation.send(kind);
    let driven = drive(
        &members,
        &generation.plan,
        external,
        <Js as Executor>::option_payload,
        &send,
    )
    .await;
    // `drive` returns at the first failed call, dropping the calls still in
    // flight: those are abandoned.
    abandon.finished = driven.is_ok();
    driven.map_err(|error| {
        let CallError {
            message,
            fatal,
            node,
        } = error;
        if fatal {
            (NodeFault::Fatal { message }, node)
        } else {
            (NodeFault::WasmTrap { message }, node)
        }
    })
}

/// Gives every member of `plan` fresh guest state through the page's
/// `rebuild`, all at once. Once every rebuild has settled, the first
/// member's in island order whose rebuild failed is the culprit: its fault
/// is [`NodeFault::Fatal`] when its error is marked `witgraphFatal` (a
/// start function called `fatal`), else [`NodeFault::Restart`].
async fn rebuild_members(
    shared: &Shared,
    plan: &IslandPlan,
) -> Result<(), (NodeFault, Option<NodeId>)> {
    let rebuilds = plan.members.iter().map(|member| {
        let node = JsValue::from_str(member.node.as_str());
        let called = shared.callbacks.rebuild.call1(&JsValue::NULL, &node);
        async move {
            match called {
                Ok(result) => settle(result).await.map(|_| ()),
                Err(error) => Err(error),
            }
        }
    });
    let rebuilt = futures::future::join_all(rebuilds).await;
    for (member, result) in plan.members.iter().zip(rebuilt) {
        if let Err(error) = result {
            let node = member.node.clone();
            let message = thrown(&error);
            let fault = if is_fatal(&error) {
                NodeFault::Fatal { message }
            } else {
                NodeFault::Restart {
                    message: format!("`{node}`: {message}"),
                }
            };
            return Err((fault, Some(node)));
        }
    }
    Ok(())
}

impl Executor for Js {
    type Value = WebValue;
    type Type = Type;
    type Island = ();

    fn run(&self, (): (), generation: Generation<WebValue>) -> GenerationFuture<()> {
        run(self.shared.clone(), generation, false).boxed_local()
    }

    fn rebuild_and_run(&self, generation: Generation<WebValue>) -> GenerationFuture<()> {
        run(self.shared.clone(), generation, true).boxed_local()
    }

    fn option_payload(value: &WebValue) -> OptionPayload<'_, WebValue> {
        match value {
            WebValue::Data(data) if data.kind() == WasmTypeKind::Option => {
                match data.unwrap_option() {
                    Some(payload) => OptionPayload::Some(Cow::Owned(WebValue::Data(Rc::new(
                        payload.into_owned(),
                    )))),
                    None => OptionPayload::None,
                }
            }
            _ => OptionPayload::NotOption,
        }
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&Type> {
        match direction {
            PortDirection::Input => self.shared.input_types.get(port),
            PortDirection::Output => self.shared.output_types.get(port),
        }
    }

    fn check_input(&self, ty: &Type, value: WebValue) -> Result<WebValue, String> {
        let WebValue::Data(data) = value else {
            return Err("a stream or future is not a Value".into());
        };
        check_type(&data, ty)?;
        Ok(WebValue::Data(data))
    }

    fn parse_wave(&self, ty: &Type, text: &str) -> Result<WebValue, String> {
        parse_wave(ty, text).map(|value| WebValue::Data(Rc::new(value)))
    }

    fn to_wave(&self, value: &WebValue) -> String {
        match value {
            WebValue::Data(value) => to_wave(value),
            WebValue::Handle(_) => String::new(),
        }
    }
}
