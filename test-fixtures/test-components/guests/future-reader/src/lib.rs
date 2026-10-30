wit_bindgen::generate!({ path: "wit", world: "future-reader" });

use exports::node::{Guest, Inputs, Outputs};

struct Reader;

impl Guest for Reader {
    async fn run(inputs: Inputs) -> Outputs {
        let value = inputs.data.await;
        Outputs { value }
    }
}

export!(Reader);
