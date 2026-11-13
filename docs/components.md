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

## Writing a node in C

`wit-bindgen c --world <world> --no-object-file wit` writes the bindings, `<world>.c` and
`<world>.h`. `run` is an async export, which C takes as a callback: `node_run` is called with
the inputs, hands the outputs to `node_run_return`, and returns `EXIT` to say it is done.

```c
#include "echo.h"

echo_callback_code_t node_run(node_inputs_t *inputs) {
  node_run_return((node_outputs_t){.out = inputs->in.is_some ? inputs->in.val : 0});
  return ECHO_CALLBACK_CODE_EXIT;
}

// Called back only for what `node_run` waits on, which is nothing.
echo_callback_code_t node_run_callback(echo_event_t *event) { __builtin_trap(); }
```

Build it for `wasm32-unknown-unknown` with clang and `wasm-ld`, then embed the world and make
it a component:

```sh
clang --target=wasm32-unknown-unknown -nostdlib -ffreestanding -I include -Wl,--no-entry \
  -o core.wasm echo_node.c echo.c rt.c
wasm-tools component embed wit core.wasm -o embedded.wasm
wasm-tools component new embedded.wasm -o echo.wasm
```

A node imports no WASI, so it has no C library. The bindings need only `realloc` and `abort`,
which the guest defines itself (in `rt.c`), with an `include/stdlib.h` declaring them. They
also call `__component_type_object_force_link_<world>`, which is in the object `wit-bindgen`
writes without `--no-object-file`: a guest that embeds its world as above defines it itself,
doing nothing. `test-fixtures/test-components/guests/c-walk` is such a node, built by the
fixtures' build script.

To stop on its lines, build its own files with `-g -O0`, from the project's directory, which the
debug info records: a debugger counts what is under it as the guest's own, and a library from
elsewhere not ([which lines are a guest's own](running.md#which-lines-are-a-guests-own)). Build
the bindings, which are under it too, without debug info (`-g0`), or exclude them by a pattern.

## Writing a node in Zig

Zig calls the C bindings as it calls any C, so a Zig node starts from what a C one does:
`wit-bindgen c --world <world> --no-object-file wit` writes them. `zig translate-c` makes the
header something Zig can import, and the node exports `node_run` and `node_run_callback` as the
C node defines them:

```zig
const c = @import("c"); // the bindings' header, as `zig translate-c` makes it

export fn node_run(inputs: *c.node_inputs_t) c.echo_callback_code_t {
    c.node_run_return(.{ .out = if (inputs.in.is_some) inputs.in.val else 0 });
    return c.ECHO_CALLBACK_CODE_EXIT;
}

// Called back only for what `node_run` waits on, which is nothing.
export fn node_run_callback(event: *c.echo_event_t) c.echo_callback_code_t {
    _ = event;
    @trap();
}
```

Build the Zig to an object, the bindings and `rt.c` as for C, and link them with `wasm-ld`:

```sh
zig translate-c -target wasm32-freestanding echo.h > c.zig
zig build-obj -target wasm32-freestanding -O Debug -fllvm \
  --dep c -Mroot=src/node.zig -Mc=c.zig -femit-bin=node.o
clang --target=wasm32-unknown-unknown -nostdlib -ffreestanding -I include -g0 -O2 -Wl,--no-entry \
  -o core.wasm node.o echo.c rt.c
wasm-tools component embed wit core.wasm -o embedded.wasm
wasm-tools component new embedded.wasm -o echo.wasm
```

`-O Debug` keeps every line to stop on. Zig's debug info says its code is C, so witgraph tells it
by its producer, and shows its values as Zig writes them: `"hello"` for a `[]const u8`,
`.{ .x = 3 }` for a struct, `null` for an empty optional, a tagged union by its active field.
Zig's standard library is not the guest's own. Zig works out at compile time what it can, and
such a value is no variable once the guest runs: make it a `var` to see it. Zig 0.17 is what
`test-fixtures/test-components/guests/zig-walk` is built with, by the fixtures' build script.

## Nodes

A component's bytes say nothing about it that witgraph reads. What it is comes
from its **description**: the WIT package its worlds are in, the worlds it
provides, and how an editor previews their outputs. A node is that, its WIT
and its builds, packed as files or as one OCI artifact.

### As files: node manifests

A node manifest (`*.node.json`) is the description, and where its WIT and
builds are, relative to it:

```json
{
  "wit": "demo:graph@0.1.0",
  "wit_source": "wit",
  "worlds": ["demo:graph/sensor@0.1.0"],
  "component": { "release": "out/sensor.wasm", "debug": "out/sensor.debug.wasm" },
  "viewers": { "demo:graph/sensor@0.1.0": { "samples": "series" } }
}
```

- `wit` is the WIT package its worlds are in, as WIT writes it.
- `wit_source` is where that WIT is: a directory or `.wit` file, with its
  `deps/` as `wkg fetch` fills it, or the package encoded in one `.wasm` file,
  as `wkg build` writes it. A component is no WIT source: its WIT is never read
  out of it.
- `worlds` lists every world of `wit` the component provides. A node runs one
  of them. A link's provider lists the world it exports, which need not be a
  node's.
- `component` is its build: one path, or a path per profile (below).
- `viewers` says how an editor previews outputs (see below).

A node manifest names files only: a node goes into a registry whole.

### As an OCI artifact

A node in a registry is an OCI image index of Wasm OCI artifacts, its
description in the index's annotations:

```text
image index   annotation  io.github.sansmoraxz.witgraph.node = {"wit":…,"worlds":…,"viewers":…}
  ├─ Wasm OCI artifact   role = wit                       its WIT package, as `wkg publish` pushes one
  ├─ Wasm OCI artifact   role = build, profile = release  a build, as `wkg oci push` pushes one
  └─ Wasm OCI artifact   role = build, profile = debug
```

(`role` and `profile` are `io.github.sansmoraxz.witgraph.role` and
`io.github.sansmoraxz.witgraph.profile`.) Every media type is a standard one:
the OCI index and manifests, and the CNCF Wasm OCI Artifact spec's config
(`application/vnd.wasm.config.v0+json`) and layer (`application/wasm`). What
witgraph adds is in annotations, which registries keep as they are given, so a
registry that only takes media types it knows (GitLab's does) holds nodes as it
holds any other artifact, and wkg can pull each entry on its own. The index
names its entries by digest, so the node's digest pins its description, its
WIT and every build. Each entry's config says `"os": "wasip3"`: a node runs on
WASI 0.3, whose async component model its contract needs, whatever it imports.

`Resolver::push` packs a node manifest's files into one, after checking it as a
resolve does, and tags it:

```rust
let pushed = Resolver::new()
    .base(dir)
    .push(Path::new("sensor.node.json"), "oci://ghcr.io/demo/sensor:0.1.0-node")
    .await?;
// pushed.reference: ghcr.io/demo/sensor@sha256:…, to pin a graph to.
```

Pushed again from the same files, a node is the same artifact, with the same
digest.

### Profiles

A node may have a build per profile: `release`, and `debug`, built with debug
info and without optimizing, for a debugger to stop on its lines. A resolver
resolves the build of its profile (`Resolver::profile`, `release` unless told)
and pulls only that one. A node of one build has it used whatever the profile;
one with builds for other profiles only fails, naming those it has. Every
build is checked to target every world listed: they share one description and
one WIT, so one contract.

### Checks

The contracts are lowered from the WIT, and the build is checked to target
each world listed, with wit-component's `targets`: the same check `wasm-tools
component targets` makes. Nothing else is read out of the bytes, so nothing a
guest embeds in them (a custom section, say) means anything to witgraph.

## Previewing outputs

A component can tell an editor how to preview its outputs, the way an image
node in a visual editor shows the image it made. Its node manifest's `viewers`
names a viewer for each output that has one, per world:

```json
"viewers": { "demo:graph/camera@0.1.0": { "frames": "image" } }
```

The viewer's name means nothing to witgraph itself. The
[editor](editor.md#previews) knows `image`, `series` and `text`, and you can
register others. A hint is only a hint: one naming an output the world doesn't
have, or no viewer, is skipped and never stops the component from loading.
What was skipped is in the catalog's `warnings`; in JavaScript, in
`session.warnings` and on the console, and the editor lists it in its
timeline.

## Describing a component

A component's description, for an editor to show beside it, is its world's
doc comment, from its WIT:

```wit
/// Adds `add` to what it is given.
world adder { /* ... */ }
```

In JavaScript it is the `docs` of each contract `describeComponent` returns.

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
for a Value, a stream of numbers, a future and a stream of bytes, each in the
`viewers.json` its manifest is written from.
