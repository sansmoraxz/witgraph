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

| Field type  | Port           | Meaning                                                                |
| ----------- | -------------- | ---------------------------------------------------------------------- |
| `T`         | Value          | Read when `run` starts (inputs) or set when `run` returns (outputs).   |
| `option<T>` | Optional value | On inputs, the port may be left unconnected.                           |
| `stream<T>` | Stream         | Ordered, back-pressured values that end when the stream closes.        |
| `future<T>` | Future         | A single value that arrives later.                                     |
| `borrow<R>` | Handle         | On inputs: a resource the host provides, lent for the length of `run`. |
| `own<R>`    | Handle         | On outputs: a resource handed to the host when `run` returns.          |

`stream`, `future` and handles can only be used directly as a field type, not
nested inside other types (an optional handle input, `option<borrow<R>>`, is
the exception). `map` and fixed-length lists can't be used in ports.

## Handles

A handle port carries a resource the host provides, such as a tensor. The
resource must be declared in a named interface the world imports, so every
node importing that interface names the same resource:

```wit
world edges {
    import test:data/tensor@0.1.0;

    export node: interface {
        use test:data/tensor@0.1.0.{tensor};
        record inputs  { image: borrow<tensor> }
        record outputs { edges: tensor }        // own<tensor>
        run: async func(inputs: inputs) -> outputs;
    }
}
```

Nodes pass only the handle. What it refers to, and where that lives (host
memory, a GPU buffer, an ML runtime's tensor), is up to the host, and so are
its operations: a node calls them, and they run where the data lives. Only a
node that reads the data into its own memory copies it.

The graph holds every shared object between runs. An input borrows its handle,
so a node can neither keep nor destroy what other nodes hold, and an output
hands its handle over. A node that wants to keep a value past its `run` asks
the resource for a reference of its own, if the resource offers one (a
`clone`, say). An object is freed when nothing holds it any more.

A handle is not text: it can't be injected or written as WAVE, and
snapshots leave handles out (see [Snapshots](running.md#snapshots)). Which
resources ports may carry is the host's to say: see
[Sharing resources](running.md#sharing-resources).

### Streams and futures of handles

A stream or future can carry owned handles, such as frames from a camera:

```wit
record inputs  { frames: stream<tensor> }  // stream<own<tensor>>
record outputs { edges: stream<tensor>, first: future<tensor> }
```

Each handle moves to whoever reads it, and that node owns it until it passes it
on or drops it. Unlike a handle port, nothing else holds it, so a stream of
handles has one reader per item, like any other stream. To send frames to two
nodes, have a node `clone` them, if the resource offers that. Handles must be at
the top of the payload: `stream<list<tensor>>` isn't supported, and the
component model doesn't allow `stream<borrow<tensor>>` at all.

Whatever is in flight is freed however the pipeline ends: when the stream runs
out, a reader stops early, nothing reads it, the graph stops, or a node faults.

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

```rust,ignore
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

```rust,ignore
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

## Describing a component

A component can say what it does, for an editor to show beside it, the way
the catalog of the [editor](editor.md) does. Its bytes keep no doc comments,
so the description lives in a custom section of the component called
`witgraph-docs`: UTF-8 text, the description whole.

```rust
macro_rules! docs {
    ($text:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-docs")]
        static DOCS: [u8; $text.len()] = *$text;
    };
}

docs!(b"Adds `add` to what it is given.");
```

It is found where a `witgraph-viewer` section is, and several sections of the
same name read as one. A section that can't be read (it is not text) is
skipped and never stops the component from loading; what was skipped is in
`Lowered::warnings`. In JavaScript the description is the `docs` of the
contract `describeComponent` returns. When a contract is lowered from WIT
source, the doc comment of its world is its description instead.

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
