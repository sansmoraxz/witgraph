wit_bindgen::generate!({ path: "wit", world: "tensor-future-source", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::{Dtype, Tensor};
use witgraph::runtime::host;

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let (tx, rx) = wit_future::new::<Tensor>(|| unreachable!("always written"));
        wit_bindgen::spawn_local(async move {
            let bytes = vec![inputs.seed.unwrap_or(0) as u8; 16 * 16 * 3];
            let tensor = Tensor::from_bytes(Dtype::Uint8, vec![16, 16, 3], bytes)
                .await
                .unwrap_or_else(|reason| {
                    host::fatal(&reason);
                    unreachable!("`fatal` does not return")
                });
            // An error gives the tensor back: dropping it frees it.
            let _ = tx.write(tensor).await;
        });
        Outputs { tensor: rx }
    }
}

export!(Source);
