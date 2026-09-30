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
}

struct StreamProducer;

impl Guest for StreamProducer {
    fn init() {}

    fn activate(_reason: ActivationKind) -> ActivationResult {
        let burst_size = runtime_host::read_value("burst-size")
            .and_then(|bytes| serde_json::from_slice::<Val>(&bytes).ok())
            .map(|v| match v {
                Val::U32(n) => n,
            })
            .unwrap_or(1);

        for i in 0..burst_size {
            let item = serde_json::to_vec(&Val::U32(i)).unwrap_or_default();
            runtime_host::push_stream("items", &item);
        }
        runtime_host::close_stream("items");
        ActivationResult::Completed
    }

    fn dispose() {}
}

bindings::export!(StreamProducer with_types_in bindings);
