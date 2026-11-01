wit_bindgen::generate!({ path: "wit", world: "blob-relay" });

use exports::node::{Guest, Inputs, Outputs};

struct Relay;

impl Guest for Relay {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs { blob: inputs.blob }
    }
}

export!(Relay);
