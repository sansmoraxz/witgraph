wit_bindgen::generate!({ path: "wit", world: "tensor-invert", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Invert;

impl Guest for Invert {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        Outputs {
            tensor: inputs.tensor.invert().await,
        }
    }
}

export!(Invert);
