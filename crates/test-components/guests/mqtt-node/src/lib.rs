wit_bindgen::generate!({ path: "wit", world: "mqtt-node", generate_all });

use exports::node::{Guest, Outputs};
use test::mqtt::source;

struct MqttNode;

impl Guest for MqttNode {
    async fn run() -> Outputs {
        let (mut tx, rx) = wit_stream::new::<u32>();
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
