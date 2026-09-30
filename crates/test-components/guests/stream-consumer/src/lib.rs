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

use std::cell::Cell;

#[derive(serde::Serialize, serde::Deserialize)]
enum Val {
    U32(u32),
}

fn read_u32_val(bytes: &[u8]) -> u32 {
    serde_json::from_slice::<Val>(bytes)
        .map(|v| match v {
            Val::U32(n) => n,
        })
        .unwrap_or(0)
}

thread_local! {
    static TOTAL: Cell<u32> = const { Cell::new(0) };
}

struct StreamConsumer;

impl Guest for StreamConsumer {
    fn init() {}

    fn activate(reason: ActivationKind) -> ActivationResult {
        match reason {
            ActivationKind::StreamItem(info) => {
                let item = read_u32_val(&info.data);
                TOTAL.with(|total| {
                    let new_total = total.get() + item;
                    total.set(new_total);
                    let out = serde_json::to_vec(&Val::U32(new_total)).unwrap_or_default();
                    runtime_host::write_value("total", &out);
                });
                ActivationResult::Continue
            }
            ActivationKind::StreamClosed(_) => ActivationResult::Completed,
            _ => ActivationResult::Continue,
        }
    }

    fn dispose() {}
}

bindings::export!(StreamConsumer with_types_in bindings);
