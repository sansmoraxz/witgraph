wit_bindgen::generate!({ path: "wit", world: "generate", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::model::model;

struct Generate;

impl Guest for Generate {
    async fn run(inputs: Inputs) -> Outputs {
        // Suspended here for as long as the host takes: the rest of the graph goes on.
        let out = model::generate(inputs.prompt.unwrap_or(0)).await;
        Outputs { out }
    }
}

export!(Generate);
