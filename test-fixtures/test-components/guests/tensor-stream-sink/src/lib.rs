wit_bindgen::generate!({ path: "wit", world: "tensor-stream-sink", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Sink;

impl Guest for Sink {
    /// Reads `take` tensors (all when absent), summing each one's first byte.
    async fn run(inputs: Inputs) -> Outputs {
        let mut input = inputs.tensors;
        let (mut count, mut sum) = (0, 0u64);
        while inputs.take.is_none_or(|n| count < n) {
            let Some(tensor) = input.next().await else {
                break;
            };
            count += 1;
            if inputs.read_last != Some(true) || inputs.take == Some(count) {
                sum += u64::from(tensor.read(0, 1).await[0]);
            }
        }
        Outputs { count, sum }
    }
}

export!(Sink);
