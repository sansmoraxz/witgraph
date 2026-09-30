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
    F64(f64),
}

struct EchoNode;

impl Guest for EchoNode {
    fn init() {}

    fn activate(_reason: ActivationKind) -> ActivationResult {
        let value = match runtime_host::read_value("in") {
            Some(bytes) => {
                serde_json::from_slice::<Val>(&bytes).unwrap_or(Val::F64(0.0))
            }
            None => Val::F64(0.0),
        };
        let out = serde_json::to_vec(&value).unwrap_or_default();
        runtime_host::write_value("out", &out);
        ActivationResult::Continue
    }

    fn dispose() {}
}

bindings::export!(EchoNode with_types_in bindings);
