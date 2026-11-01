wit_bindgen::generate!({ path: "wit", world: "tensor-keep", generate_all });

use std::cell::RefCell;

use exports::node::{Guest, Inputs, Outputs};
use test::data::tensor::Tensor;

thread_local! {
    /// The last input: this node's own reference to it, which outlives the
    /// borrow its `run` was given.
    static KEPT: RefCell<Option<Tensor>> = const { RefCell::new(None) };
}

struct Keep;

impl Guest for Keep {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        let kept = inputs.tensor.map(Tensor::clone);
        let previous = KEPT.with(|cell| cell.replace(kept)).map_or(0, |previous| {
            let len: u64 = previous.shape().iter().map(|&d| u64::from(d)).product();
            previous.read(0, len).iter().map(|&b| u64::from(b)).sum()
        });
        Outputs { previous }
    }
}

export!(Keep);
