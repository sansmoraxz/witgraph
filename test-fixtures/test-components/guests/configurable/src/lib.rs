wit_bindgen::generate!({ path: "wit", world: "configurable" });

use exports::node::{Guest, Inputs, Outputs};

/// Declares how an editor previews this node's outputs (the
/// `witgraph-viewer` custom section).
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.result=series\n");

struct Configurable;

impl Guest for Configurable {
    async fn run(inputs: Inputs) -> Outputs {
        let rate = inputs.rate.unwrap_or(1);
        let gain = inputs.gain.unwrap_or(1.0);
        Outputs {
            result: f64::from(rate) * gain,
        }
    }
}

export!(Configurable);
