wit_bindgen::generate!({ path: "wit", world: "reading-consumer" });

use exports::node::{Guest, Inputs, Outputs};

struct Consumer;

impl Guest for Consumer {
    async fn run(inputs: Inputs) -> Outputs {
        let mut items = inputs.items;
        let mut total = 0u32;
        let mut count = 0u32;
        while let Some(reading) = items.next().await {
            total = total.wrapping_add(reading.value);
            count += 1;
        }
        Outputs { total, count }
    }
}

export!(Consumer);
