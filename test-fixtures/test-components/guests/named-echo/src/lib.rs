wit_bindgen::generate!({ path: "wit", world: "named-echo" });

use exports::test::named::node::{Guest, Inputs, Outputs};

struct NamedEcho;

impl Guest for NamedEcho {
    fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: inputs.in_.unwrap_or(0.0) * 2.0,
        }
    }
}

export!(NamedEcho);
