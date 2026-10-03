wit_bindgen::generate!({ path: "wit", world: "stream-producer" });

use exports::node::{Guest, Inputs, Outputs};

struct Producer;

impl Guest for Producer {
    async fn run(inputs: Inputs) -> Outputs {
        let limit = inputs.burst_size.unwrap_or(u32::MAX);
        let (mut tx, rx) = wit_stream::new::<u32>();
        // Return the reader first and write afterwards: the consumer can
        // only start once this `run` has handed over its stream.
        wit_bindgen::spawn_local(async move {
            let mut i = 0u32;
            while inputs.burst_size.is_none_or(|n| i < n) {
                // `Some` back means the reader was dropped.
                if tx.write_one(i).await.is_some() {
                    break;
                }
                i = i.wrapping_add(1);
            }
        });
        Outputs { items: rx, limit }
    }
}

export!(Producer);
