wit_bindgen::generate!({ path: "wit", world: "calc", generate_all });

use exports::node::{Guest, Inputs, Outputs};
use test::math::ops;

struct Calc;

impl Guest for Calc {
    async fn run(inputs: Inputs) -> Outputs {
        let x = inputs.x.unwrap_or(0);
        let doubled = ops::double(x);
        let slow = ops::slow_double(x).await;
        let mut items = ops::count(x).await;
        let mut total = 0;
        while let Some(item) = items.next().await {
            total += item;
        }
        Outputs {
            doubled,
            slow,
            total,
            calls: ops::calls(),
        }
    }
}

export!(Calc);
