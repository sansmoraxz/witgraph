wit_bindgen::generate!({ path: "wit", world: "tensor-sink", generate_all });

use exports::node::{Guest, Inputs, Outputs};

/// How many bytes one `read` copies in: a sink need not hold a whole
/// tensor at once.
const WINDOW: u64 = 64 * 1024;

struct Sink;

impl Guest for Sink {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        let (mut sum, mut len) = (0u64, 0u64);
        loop {
            let window = inputs.tensor.read(len, WINDOW);
            if window.is_empty() {
                break;
            }
            len += window.len() as u64;
            sum += window.iter().map(|&b| u64::from(b)).sum::<u64>();
        }
        Outputs { sum, len }
    }
}

export!(Sink);
