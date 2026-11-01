# Components

A witgraph component is a WIT world that exports an interface named `node`.
That interface has up to two records, `inputs` and `outputs` (at least one),
and one function, `run`:

```wit
package demo:graph@0.1.0;

world sensor {
    import demo:caps/clock@0.1.0;          // something the host provides

    export node: interface {
        record reading { value: f64, timestamp: u64 }

        record inputs {
            sample-rate: option<u32>,      // optional input
        }
        record outputs {
            latest: reading,               // a single value
            samples: stream<reading>,      // a stream of values
        }

        run: async func(inputs: inputs) -> outputs;
    }
}
```

## Ports

Every field of `inputs` and `outputs` is a port. The field's type decides
what kind of port it is:

| Field type  | Port           | Meaning                                                              |
| ----------- | -------------- | -------------------------------------------------------------------- |
| `T`         | Value          | Read when `run` starts (inputs) or set when `run` returns (outputs). |
| `option<T>` | Optional value | On inputs, the port may be left unconnected.                         |
| `stream<T>` | Stream         | Ordered, back-pressured values that end when the stream closes.      |
| `future<T>` | Future         | A single value that arrives later.                                   |

`stream` and `future` can only be used directly as a field type, not nested
inside other types. Resources, `map` and fixed-length lists can't be used in
ports.

## `run`

`run` must be an `async func` that takes `inputs` and returns `outputs`. It can
wait on its inputs, for example by reading a whole stream and returning a
total. To produce a stream, return the reader and keep writing from a spawned
task. Writing before returning would deadlock, because consumers only start
once `run` has returned.

## Capabilities

Anything a component imports that the host must supply, such as a clock or a
key-value store, is a capability. Capabilities are part of the component's
contract, so a graph can't be run on a host that lacks them. The built-in
`witgraph:runtime/host` interface, which has `fatal(message)` to stop the
graph, is always available.

## Writing a node in Rust

```rust
wit_bindgen::generate!({ path: "wit", world: "echo" });

use exports::node::{Guest, Inputs, Outputs};

struct Echo;

impl Guest for Echo {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs { out: inputs.in_.unwrap_or(0.0) }
    }
}

export!(Echo);
```

A node that produces a stream returns the reader and writes from a spawned
task:

```rust
impl Guest for Producer {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<u32>();
        wit_bindgen::spawn_local(async move {
            for i in 0.. {
                // Writing fails once the reader is dropped.
                if tx.write_one(i).await.is_some() {
                    break;
                }
            }
        });
        Outputs { items: rx }
    }
}
```

If the world imports other packages, put their WIT under `wit/deps/` and pass
`generate_all` to `generate!`.
