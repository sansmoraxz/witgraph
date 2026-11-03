wit_bindgen::generate!({ path: "wit", world: "corpus-relay" });

use exports::node::{Guest, Inputs, Outputs};

struct Relay;

impl Guest for Relay {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            value: inputs.value,
            other: inputs.other,
        }
    }
}

export!(Relay);
