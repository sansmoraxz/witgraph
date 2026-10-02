wit_bindgen::generate!({ path: "wit", world: "relay" });

use exports::node::{Guest, Inputs, Outputs};

struct Relay;

impl Guest for Relay {
    fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: inputs.in_.wrapping_add(inputs.add.unwrap_or(0)),
        }
    }
}

export!(Relay);
