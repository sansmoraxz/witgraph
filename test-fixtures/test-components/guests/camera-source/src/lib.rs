wit_bindgen::generate!({ path: "wit", world: "camera-source", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::camera::camera;
use test::data::tensor::Tensor;
use witgraph::runtime::host;

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Tensor>();
        wit_bindgen::spawn_local(async move {
            let mut i = 0u32;
            while inputs.frames.is_none_or(|n| i < n) {
                let frame = camera::next_frame().await.unwrap_or_else(|reason| {
                    host::fatal(&reason);
                    unreachable!("`fatal` does not return")
                });
                // `Some` back means the reader was dropped.
                if tx.write_one(frame).await.is_some() {
                    break;
                }
                i = i.wrapping_add(1);
            }
        });
        Outputs { tensors: rx }
    }
}

export!(Source);
