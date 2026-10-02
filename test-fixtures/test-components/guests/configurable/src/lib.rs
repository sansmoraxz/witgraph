wit_bindgen::generate!({ path: "wit", world: "configurable" });

use exports::node::{Guest, Inputs, Outputs};

struct Configurable;

impl Guest for Configurable {
    fn run(inputs: Inputs) -> Outputs {
        let rate = inputs.rate.unwrap_or(1);
        let gain = inputs.gain.unwrap_or(1.0);
        Outputs {
            result: f64::from(rate) * gain,
        }
    }
}

export!(Configurable);
