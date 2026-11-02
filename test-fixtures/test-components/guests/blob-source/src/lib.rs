wit_bindgen::generate!({ path: "wit", world: "blob-source" });

use exports::node::{Guest, Inputs, Outputs};

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let seed = inputs.seed.unwrap_or(0);
        let blob = (0..inputs.size.unwrap_or(0))
            .map(|i| i.wrapping_add(seed) as u8)
            .collect();
        Outputs { blob }
    }
}

export!(Source);
