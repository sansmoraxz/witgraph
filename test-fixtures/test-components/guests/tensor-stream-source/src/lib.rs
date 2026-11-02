wit_bindgen::generate!({ path: "wit", world: "tensor-stream-source", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::{Dtype, Tensor};
use witgraph::runtime::host;

async fn make(width: u32, height: u32, fill: u8) -> Tensor {
    let bytes = vec![fill; (width * height * 3) as usize];
    Tensor::from_bytes(Dtype::Uint8, vec![height, width, 3], bytes)
        .await
        .unwrap_or_else(|reason| {
            host::fatal(&reason);
            unreachable!("`fatal` does not return")
        })
}

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

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Tensor>();
        wit_bindgen::spawn_local(async move {
            let (width, height) = (inputs.width.unwrap_or(16), inputs.height.unwrap_or(16));
            let first = match inputs.reuse {
                Some(true) => Some(make(width, height, 0).await),
                _ => None,
            };
            let mut i = 0u32;
            while inputs.frames.is_none_or(|n| i < n) {
                let tensor = match &first {
                    Some(tensor) => tensor.clone(),
                    None => make(width, height, i as u8).await,
                };
                // `Some` back means the reader was dropped: the tensor is ours
                // again, and dropping it frees it.
                if tx.write_one(tensor).await.is_some() {
                    break;
                }
                i = i.wrapping_add(1);
            }
        });
        Outputs { tensors: rx }
    }
}

export!(Source);
