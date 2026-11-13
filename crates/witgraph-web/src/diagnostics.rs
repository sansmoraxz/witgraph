//! What is wrong with a graph, as data a JavaScript host can act on: each diagnostic
//! with a stable code, and the place in the graph it points at.

use js_sys::{Array, Object, Reflect};
use miette::Diagnostic as _;
use wasm_bindgen::prelude::*;
use witgraph_ir::{CompilationFailure, ComponentContract, Diagnostic, Graph, Location};

use crate::graph::{error, text};
use crate::value::set;

/// `names`, as an array of text.
fn texts<'a>(names: impl IntoIterator<Item = &'a str>) -> Array {
    names.into_iter().map(text).collect()
}

/// Where a diagnostic points: its `kind`, and always the `nodes` it touches, so a
/// host marking nodes need not tell the kinds apart.
fn location_object(location: &Location) -> Object {
    let object = Object::new();
    let nodes = |object: &Object, kind: &str, ids: Array| {
        set(object, "kind", &text(kind));
        set(object, "nodes", &ids);
    };
    match location {
        Location::Graph => nodes(&object, "graph", Array::new()),
        Location::Component(component) => {
            nodes(&object, "component", Array::new());
            set(&object, "component", &text(&component.to_string()));
        }
        Location::Node(node) => nodes(&object, "node", texts([node.as_str()])),
        Location::Nodes(ids) => {
            nodes(&object, "nodes", texts(ids.iter().map(|n| n.as_str())));
        }
        Location::Port(port) => {
            nodes(&object, "port", texts([port.node.as_str()]));
            set(&object, "port", &text(port.port.as_str()));
        }
        Location::Connection(connection) => {
            nodes(&object, "connection", Array::new());
            set(&object, "connection", &text(connection.as_str()));
        }
        Location::Link(link) => {
            nodes(&object, "link", Array::new());
            set(&object, "link", &text(link.as_str()));
        }
        Location::Cycle(ids) => {
            nodes(&object, "cycle", texts(ids.iter().map(|n| n.as_str())));
        }
        Location::Group(ids) => {
            nodes(&object, "group", texts(ids.iter().map(|n| n.as_str())));
        }
    }
    object
}

/// `{ code, severity, message, help?, location }`.
pub(crate) fn diagnostic_object(diagnostic: &Diagnostic) -> JsValue {
    let object = Object::new();
    if let Some(code) = diagnostic.code() {
        set(&object, "code", &text(&code.to_string()));
    }
    let severity = if diagnostic.is_error() {
        "error"
    } else {
        "warning"
    };
    set(&object, "severity", &text(severity));
    set(&object, "message", &text(&diagnostic.to_string()));
    if let Some(help) = diagnostic.help() {
        set(&object, "help", &text(&help.to_string()));
    }
    set(
        &object,
        "location",
        &location_object(&diagnostic.location()),
    );
    object.into()
}

pub(crate) fn diagnostic_array<'a>(diagnostics: impl Iterator<Item = &'a Diagnostic>) -> Array {
    diagnostics.map(diagnostic_object).collect()
}

/// A failed compile as an `Error`: its message lists the errors, and its `diagnostics`
/// say all of it as data, warnings too.
pub(crate) fn compile_error(failure: &CompilationFailure) -> JsValue {
    let list: Vec<String> = failure
        .diagnostics
        .errors()
        .map(ToString::to_string)
        .collect();
    let thrown = js_sys::Error::new(&format!("the graph does not compile:\n{}", list.join("\n")));
    let diagnostics = diagnostic_array(failure.diagnostics.iter());
    // Setting a field of an `Error` just made cannot fail.
    let _ = Reflect::set(&thrown, &JsValue::from_str("diagnostics"), &diagnostics);
    thrown.into()
}

/// Compiles graphs against a set of components, with nothing loaded: what an editor
/// asks of a graph it has not made yet, by the rules a load goes by.
#[wasm_bindgen]
pub struct GraphChecker {
    contracts: Vec<ComponentContract>,
}

#[wasm_bindgen]
impl GraphChecker {
    /// `components` as `WebGraph.load` takes them, bytes or none: their contracts
    /// are their descriptions' and WIT's. One that does not describe is left out: a
    /// graph with a node of it says so.
    #[wasm_bindgen(constructor)]
    pub fn new(components: Array) -> Result<GraphChecker, JsError> {
        let mut contracts = Vec::new();
        for entry in components.iter() {
            if let Ok(entry) = crate::component::entry(&entry) {
                contracts.extend(entry.described.worlds.iter().map(|w| w.contract.clone()));
            }
        }
        Ok(Self { contracts })
    }

    /// Everything a compile says of `graph`, as JSON: errors and warnings alike.
    /// Graph JSON that does not parse throws.
    pub fn check(&self, graph: &str) -> Result<Array, JsError> {
        let graph: Graph = serde_json::from_str(graph).map_err(error)?;
        Ok(match graph.compile(&self.contracts) {
            Ok(compiled) => diagnostic_array(compiled.warnings().iter()),
            Err(failure) => diagnostic_array(failure.diagnostics.iter()),
        })
    }
}
