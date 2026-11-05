wit_bindgen::generate!({ path: "wit", world: "line-stream" });

use exports::node::{Guest, Inputs, Outputs};

struct LineStream;

impl Guest for LineStream {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut items } = inputs;
        let (mut tx, rx) = wit_stream::new::<u32>();
        wit_bindgen::spawn_local(async move {
            while let Some(item) = items.next().await { // mark: read
                let doubled = core::hint::black_box(item * 2); // mark: body
                if tx.write_one(doubled).await.is_some() { // mark: write
                    break;
                }
            }
            drop(items);
        });
        Outputs { items: rx }
    }
}

export!(LineStream);
