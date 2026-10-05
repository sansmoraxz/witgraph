wit_bindgen::generate!({ path: "wit", world: "labelled", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Labelled;

impl Guest for Labelled {
    async fn run(inputs: Inputs) -> Outputs {
        let key = inputs.key.unwrap_or_default();
        Outputs {
            primary: primary::get(&key),
            backup: backup::get(&key),
        }
    }
}

export!(Labelled);
