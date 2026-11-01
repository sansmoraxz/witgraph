wit_bindgen::generate!({ path: "wit", world: "mqtt-node", generate_all });

use exports::node::{Guest, Outputs};
use test::mqtt::source;

/// Declares how an editor previews this node's outputs (the
/// `witgraph-viewer` custom section).
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.messages=text\n");

struct MqttNode;

impl Guest for MqttNode {
    async fn run() -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Vec<u8>>();
        wit_bindgen::spawn_local(async move {
            while let Some(message) = source::next_message() {
                if tx.write_one(message).await.is_some() {
                    break;
                }
            }
        });
        Outputs { messages: rx }
    }
}

export!(MqttNode);
