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

## Previewing outputs

A component can tell an editor how to preview its outputs, the way an image
node in a visual editor shows the image it made. The hints live in a custom
section of the component called `witgraph-viewer`, as UTF-8 text with one
line per output:

```text
# comments and blank lines are ignored
output.frames=image
```

The viewer's name means nothing to witgraph itself. The
[editor](editor.md#previews) knows `image`, `series` and `text`, and you can
register others. A component can only name its own outputs, each one once.
A hint is only a hint: a line that can't be parsed, or names an output the
component doesn't have, is skipped and never stops the component from
loading. What was skipped is in `Lowered::warnings` and the catalog's
`warnings`; in JavaScript, in `session.warnings` and on the console, and the
editor lists it in its timeline.

In Rust, embed the section with a static. It ends up in the component's core
module, which is where witgraph looks for it (a section inside a component
composed into this one, such as a link's provider, belongs to that one):

```rust
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.frames=image\n");
```

The linker joins sections with the same name, so end every line, the last
included, with a newline.

The `image` viewer shows either a record of raw pixels or the bytes of an
encoded image:

```wit
/// `width` x `height` pixels, row by row, four bytes (RGBA) each.
record frame {
    width: u32,
    height: u32,
    pixels: list<u8>,
}
```

or a `list<u8>` holding a PNG, JPEG, GIF or WebP file. The output can be a
Value, a stream or a future: the editor shows its latest value, item or
resolution.

`series` plots numbers (a number, a bool, or a record's first number), one
point per tick for a Value output and per item for a stream. `text` prints
anything: a Value's value, a future's (or that it is pending), and a
stream's item count with its latest items. Bytes print as text when they are
printable, as hex when not.

The video pipeline among the test components (`video-source`,
`frame-grayscale`, `frame-invert` and `video-sink`) is a complete example;
`relay`, `stream-producer`, `future-writer` and `mqtt-node` declare viewers
for a Value, a stream of numbers, a future and a stream of bytes.
