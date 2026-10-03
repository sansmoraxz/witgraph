wit_bindgen::generate!({ path: "wit", world: "maybe" });

use exports::node::{Guest, Inputs, Outputs};

struct Maybe;

impl Guest for Maybe {
    fn run(inputs: Inputs) -> Outputs {
        Outputs { maybe: inputs.x }
    }
}

export!(Maybe);
