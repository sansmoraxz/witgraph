wit_bindgen::generate!({ path: "wit", world: "bytes-consumer" });

use exports::node::{Guest, Inputs, Outputs};

struct Consumer;

impl Guest for Consumer {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut items } = inputs;
        let mut count = 0u32;
        let mut total_bytes = 0u32;
        while let Some(msg) = items.next().await {
            count += 1;
            total_bytes = total_bytes.wrapping_add(msg.len() as u32);
        }
        drop(items);
        Outputs { count, total_bytes }
    }
}

export!(Consumer);
