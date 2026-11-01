wit_bindgen::generate!({ path: "wit", world: "relay" });

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

viewer!(b"output.out=series\n");

struct Relay;

impl Guest for Relay {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs {
            out: inputs.in_.wrapping_add(inputs.add.unwrap_or(0)),
        }
    }
}

export!(Relay);
