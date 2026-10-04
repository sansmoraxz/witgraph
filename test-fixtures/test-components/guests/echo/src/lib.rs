wit_bindgen::generate!({ path: "wit", world: "echo" });

use exports::node::{Guest, Inputs, Outputs};

struct Echo;

impl Guest for Echo {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: inputs.in_.unwrap_or(0.0),
        }
    }
}

export!(Echo);
