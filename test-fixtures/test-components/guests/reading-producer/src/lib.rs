wit_bindgen::generate!({ path: "wit", world: "reading-producer" });

use exports::node::{Guest, Inputs, Outputs, Reading};

struct Producer;

impl Guest for Producer {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Reading>();
        wit_bindgen::spawn_local(async move {
            let mut value = 0u32;
            while inputs.burst_size.is_none_or(|n| value < n) {
                // `Some` back means the reader was dropped.
                if tx.write_one(Reading { value }).await.is_some() {
                    break;
                }
                value = value.wrapping_add(1);
            }
        });
        Outputs { items: rx }
    }
}

export!(Producer);
