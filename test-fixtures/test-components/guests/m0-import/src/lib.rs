wit_bindgen::generate!({ path: "wit", world: "m0-import" });

use exports::node::{Guest, Inputs, Outputs};

struct M0Import;

impl Guest for M0Import {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: m0().wrapping_add(inputs.x.unwrap_or(0)),
        }
    }
}

export!(M0Import);
