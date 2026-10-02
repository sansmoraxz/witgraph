wit_bindgen::generate!({ path: "wit", world: "busy-loop", generate_all });

use exports::node::{Action, Guest, Inputs, Outputs};
use witgraph::runtime::host;

struct BusyLoop;

impl Guest for BusyLoop {
    async fn run(inputs: Inputs) -> Outputs {
        match inputs.action.unwrap_or(Action::Finish) {
            Action::Finish => {}
            Action::Spin => {
                let mut x = 0u64;
                loop {
                    x = core::hint::black_box(x.wrapping_add(1));
                }
            }
            Action::Trap => core::arch::wasm32::unreachable(),
            Action::Fatal => host::fatal("busy-loop asked for it"),
        }
        Outputs { done: 1 }
    }
}

export!(BusyLoop);
