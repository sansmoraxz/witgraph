wit_bindgen::generate!({ path: "wit", world: "stream-consumer" });

use exports::node::{Guest, Inputs, Outputs};

struct Consumer;

impl Guest for Consumer {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut items, take, delay } = inputs;
        let mut total = 0u32;
        let mut count = 0u32;
        while take.is_none_or(|t| count < t) {
            let Some(item) = items.next().await else { break };
            total = total.wrapping_add(item);
            count += 1;
            for _ in 0..delay.unwrap_or(0) {
                wit_bindgen::yield_async().await;
            }
        }
        // Dropping the reader tells the producer to stop.
        drop(items);
        Outputs { total, count }
    }
}

export!(Consumer);
