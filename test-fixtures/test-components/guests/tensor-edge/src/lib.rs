wit_bindgen::generate!({ path: "wit", world: "tensor-edge", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use witgraph::runtime::host;

/// A Laplacian: bright where the image changes, dark where it is flat.
const KERNEL: [f32; 9] = [-1.0, -1.0, -1.0, -1.0, 8.0, -1.0, -1.0, -1.0, -1.0];

struct Edge;

impl Guest for Edge {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        let edges = inputs
            .tensor
            .conv3x3(KERNEL.to_vec())
            .await
            .unwrap_or_else(|reason| {
                host::fatal(&reason);
                unreachable!("`fatal` does not return")
            });
        Outputs { edges }
    }
}

export!(Edge);
