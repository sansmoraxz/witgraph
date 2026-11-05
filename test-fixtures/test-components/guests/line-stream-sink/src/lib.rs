wit_bindgen::generate!({ path: "wit", world: "line-stream-sink" });

use exports::node::{Guest, Inputs, Outputs};

struct LineStreamSink;

impl Guest for LineStreamSink {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut items } = inputs;
        let mut count = 0;
        while let Some(item) = items.next().await { // mark: read
            count += core::hint::black_box(item).min(1); // mark: body
        }
        Outputs { count }
    }
}

export!(LineStreamSink);
