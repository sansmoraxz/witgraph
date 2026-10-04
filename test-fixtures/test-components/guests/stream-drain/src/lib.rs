wit_bindgen::generate!({ path: "wit", world: "stream-drain" });

use exports::node::{Guest, Inputs, Outputs};

struct Drain;

impl Guest for Drain {
    async fn run(inputs: Inputs) -> Outputs {
        let mut items = inputs.items;
        wit_bindgen::spawn_local(async move { while items.next().await.is_some() {} });
        Outputs { started: true }
    }
}

export!(Drain);
