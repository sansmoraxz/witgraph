wit_bindgen::generate!({ path: "wit", world: "relay" });

use exports::node::{Guest, Inputs, Outputs};

struct RelayWide;

impl Guest for RelayWide {
    async fn run(inputs: Inputs) -> Outputs {
        let out = inputs.in_.wrapping_add(inputs.add.unwrap_or(0));
        Outputs {
            out,
            doubled: out.wrapping_mul(2),
        }
    }
}

export!(RelayWide);
