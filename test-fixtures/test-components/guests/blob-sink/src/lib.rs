wit_bindgen::generate!({ path: "wit", world: "blob-sink" });

use exports::node::{Guest, Inputs, Outputs};

struct Sink;

impl Guest for Sink {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            len: inputs.blob.len() as u32,
            last: inputs.blob.last().copied().unwrap_or(0),
        }
    }
}

export!(Sink);
