wit_bindgen::generate!({ path: "wit", world: "tensor-source", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::{Dtype, Tensor};

/// Declares how an editor previews this node's outputs (the
/// `witgraph-viewer` custom section).
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.tensor=tensor\n");

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let width = inputs.width.unwrap_or(64);
        let height = inputs.height.unwrap_or(48);
        let seed = inputs.seed.unwrap_or(0);
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let shift = |v: u32| (v.wrapping_add(seed) % 256) as u8;
                pixels.extend([shift(x * 255 / width.max(1)), shift(y * 255 / height.max(1)), shift(128), 255]);
            }
        }
        let tensor = Tensor::from_bytes(Dtype::Uint8, &[height, width, 4], &pixels)
            .unwrap_or_else(|e| panic!("from-bytes: {e}"));
        Outputs { tensor }
    }
}

export!(Source);
