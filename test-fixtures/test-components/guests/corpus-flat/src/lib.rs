wit_bindgen::generate!({ path: "wit", world: "corpus-flat" });

use exports::node::{Guest, Inputs, Outputs};

struct Relay;

impl Guest for Relay {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            mixed: inputs.mixed,
        }
    }
}

export!(Relay);
