wit_bindgen::generate!({ path: "wit", world: "tensor-future-sink", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Sink;

impl Guest for Sink {
    /// The tensor's first byte, or 1000 when told to `ignore` the future.
    async fn run(inputs: Inputs) -> Outputs {
        if inputs.ignore == Some(true) {
            drop(inputs.tensor);
            return Outputs { first: 1000 };
        }
        let tensor = inputs.tensor.await;
        Outputs {
            first: u32::from(tensor.read(0, 1).await[0]),
        }
    }
}

export!(Sink);
