wit_bindgen::generate!({ path: "wit", world: "tensor-stream-invert", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::Tensor;

struct Invert;

impl Guest for Invert {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Tensor>();
        let mut input = inputs.tensors;
        wit_bindgen::spawn_local(async move {
            while let Some(tensor) = input.next().await {
                let inverted = tensor.invert().await;
                drop(tensor);
                if tx.write_one(inverted).await.is_some() {
                    break;
                }
            }
        });
        Outputs { tensors: rx }
    }
}

export!(Invert);
