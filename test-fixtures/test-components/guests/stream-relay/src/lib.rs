wit_bindgen::generate!({ path: "wit", world: "stream-relay" });

use exports::node::{Guest, Inputs, Outputs};

struct Relay;

impl Guest for Relay {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            items: inputs.items,
        }
    }
}

export!(Relay);
