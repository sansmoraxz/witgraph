wit_bindgen::generate!({ path: "wit", world: "tensor-stream-edge", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::Tensor;
use witgraph::runtime::host;

/// Declares how an editor previews this node's outputs (the
/// `witgraph-viewer` custom section).
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.tensors=tensor\n");

/// A Laplacian: bright where the image changes, dark where it is flat.
const KERNEL: [f32; 9] = [-1.0, -1.0, -1.0, -1.0, 8.0, -1.0, -1.0, -1.0, -1.0];

struct Edge;

impl Guest for Edge {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Tensor>();
        let mut input = inputs.tensors;
        wit_bindgen::spawn_local(async move {
            while let Some(tensor) = input.next().await {
                let edges = tensor
                    .conv3x3(KERNEL.to_vec())
                    .await
                    .unwrap_or_else(|reason| {
                        host::fatal(&reason);
                        unreachable!("`fatal` does not return")
                    });
                drop(tensor);
                if tx.write_one(edges).await.is_some() {
                    break;
                }
            }
        });
        Outputs { tensors: rx }
    }
}

export!(Edge);
