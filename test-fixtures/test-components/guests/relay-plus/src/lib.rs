wit_bindgen::generate!({ path: "wit", world: "relay" });

use exports::node::{Guest, Inputs, Outputs};

struct RelayPlus;

impl Guest for RelayPlus {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: inputs
                .in_
                .wrapping_add(inputs.add.unwrap_or(0))
                .wrapping_add(1),
        }
    }
}

export!(RelayPlus);
