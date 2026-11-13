wit_bindgen::generate!({ path: "wit", world: "tensor-copy-invert", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::Tensor;

struct CopyInvert;

impl Guest for CopyInvert {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        let input = inputs.tensor;
        let shape = input.shape();
        let len: u64 = shape.iter().map(|&d| u64::from(d)).product();
        let mut bytes = input.read(0, len).await;
        for byte in &mut bytes {
            *byte = 255 - *byte;
        }
        let tensor = Tensor::from_bytes(input.dtype(), shape, bytes)
            .await
            .unwrap_or_else(|e| panic!("from-bytes: {e}"));
        Outputs { tensor }
    }
}

export!(CopyInvert);
