wit_bindgen::generate!({ path: "wit", world: "future-dropper", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Dropper;

impl Guest for Dropper {
    async fn run(inputs: Inputs) -> Outputs {
        drop(inputs.data);
        Outputs { done: true }
    }
}

export!(Dropper);
