#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../witgraph-runtime/wit",
        world: "graph-node",
    });
}

use bindings::exports::witgraph::runtime::node::Guest;
use bindings::witgraph::runtime::runtime_host;
use bindings::witgraph::runtime::types::{ActivationKind, ActivationResult};

#[derive(serde::Serialize, serde::Deserialize)]
enum Val {
    U32(u32),
    F64(f64),
}

fn read_u32(port: &str, default: u32) -> u32 {
    runtime_host::read_value(port)
        .and_then(|bytes| serde_json::from_slice::<Val>(&bytes).ok())
        .map(|v| match v {
            Val::U32(n) => n,
            Val::F64(n) => n as u32,
        })
        .unwrap_or(default)
}

fn read_f64(port: &str, default: f64) -> f64 {
    runtime_host::read_value(port)
        .and_then(|bytes| serde_json::from_slice::<Val>(&bytes).ok())
        .map(|v| match v {
            Val::F64(n) => n,
            Val::U32(n) => n as f64,
        })
        .unwrap_or(default)
}

struct ConfigurableNode;

impl Guest for ConfigurableNode {
    fn init() {}

    fn activate(_reason: ActivationKind) -> ActivationResult {
        let rate = read_u32("rate", 1);
        let gain = read_f64("gain", 1.0);
        let result = Val::F64(rate as f64 * gain);
        let out = serde_json::to_vec(&result).unwrap_or_default();
        runtime_host::write_value("result", &out);
        ActivationResult::Continue
    }

    fn dispose() {}
}

bindings::export!(ConfigurableNode with_types_in bindings);
