#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        inline: r#"
            package witgraph:runtime@0.1.0;

            interface types {
                variant activation-kind {
                    sync,
                    drain-item(drain-item-info),
                    stream-item(port-item-info),
                    event(port-item-info),
                    future-resolved(port-item-info),
                    stream-closed(string),
                }
                record drain-item-info { port: string, data: list<u8> }
                record port-item-info { port: string, data: list<u8> }
                enum activation-result { %continue, completed }
            }

            interface runtime-host {
                use types.{activation-kind, activation-result, drain-item-info, port-item-info};
                read-value: func(port: string) -> option<list<u8>>;
                write-value: func(port: string, value: list<u8>);
                emit-event: func(port: string, payload: list<u8>);
                push-stream: func(port: string, item: list<u8>);
                close-stream: func(port: string);
                resolve-future: func(port: string, value: list<u8>);
                fatal: func(message: string);
            }

            interface node {
                use types.{activation-kind, activation-result};
                init: func();
                activate: func(reason: activation-kind) -> activation-result;
                dispose: func();
            }

            interface mqtt-source {
                next-message: func() -> option<list<u8>>;
            }

            world mqtt-graph-node {
                import runtime-host;
                import mqtt-source;
                export node;
            }
        "#,
        world: "mqtt-graph-node",
    });
}

use bindings::exports::witgraph::runtime::node::Guest;
use bindings::witgraph::runtime::mqtt_source;
use bindings::witgraph::runtime::runtime_host;
use bindings::witgraph::runtime::types::{ActivationKind, ActivationResult};

#[derive(serde::Serialize, serde::Deserialize)]
enum Val {
    U32(u32),
}

struct MqttNode;

impl Guest for MqttNode {
    fn init() {}

    fn activate(_reason: ActivationKind) -> ActivationResult {
        let mut count: u32 = 0;
        while mqtt_source::next_message().is_some() {
            count += 1;
        }
        let out = serde_json::to_vec(&Val::U32(count)).unwrap_or_default();
        runtime_host::write_value("count", &out);
        ActivationResult::Continue
    }

    fn dispose() {}
}

bindings::export!(MqttNode with_types_in bindings);
