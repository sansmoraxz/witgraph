wit_bindgen::generate!({ path: "wit", world: "tensor-invert", generate_all });

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

viewer!(b"output.tensor=tensor\n");

struct Invert;

impl Guest for Invert {
    async fn run(inputs: Inputs<'_>) -> Outputs {
        Outputs {
            tensor: inputs.tensor.invert().await,
        }
    }
}

export!(Invert);
