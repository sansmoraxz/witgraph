# witgraph

Build dataflow graphs out of WebAssembly components, then run them.

Each node in a graph is a WASM component. The component's WIT world says what
the node takes in, what it produces and what it needs from the host, so there
is no separate schema to keep in sync: witgraph reads it from the WIT. Graphs
are checked against those contracts before they run, and run on
[wasmtime](https://wasmtime.dev) (or in a browser) using the component model's
`stream<T>` and `future<T>`.

## A node

A node is a WIT world that exports an interface called `node`. Its input and
output records are the node's ports, and `run` is the function that does the
work:

```wit
package demo:graph@0.1.0;

world scale {
    export node: interface {
        record inputs {
            value: f64,
            factor: option<f64>,    // optional input
        }
        record outputs {
            out: f64,
        }

        run: async func(inputs: inputs) -> outputs;
    }
}
```

In Rust, with `wit-bindgen`:

```rust,ignore
wit_bindgen::generate!({ path: "wit", world: "scale" });

use exports::node::{Guest, Inputs, Outputs};

struct Scale;

impl Guest for Scale {
    async fn run(inputs: Inputs) -> Outputs {
        Outputs { out: inputs.value * inputs.factor.unwrap_or(1.0) }
    }
}

export!(Scale);
```

A field of type `T` is a plain value. `stream<T>` and `future<T>` fields carry
data over time. See [docs/components.md](docs/components.md) for the full rules.

## A graph

A graph lists the components it uses, by name with where each one's node is
(a node manifest, or an OCI node artifact), the nodes (each runs one world of one component), and the
connections between their ports. It is plain serializable data and holds no
contracts. Compiling it looks the contracts up
and reports every problem it finds: mismatched types, unconnected required
inputs, cycles without a feedback edge, and so on.

```rust
let compiled = graph.compile(&contracts)?;
```

`witgraph-wit` lowers the contracts from WIT: a node's description names its
WIT package and the worlds it provides, and its builds are only checked to
target them. See [docs/graphs.md](docs/graphs.md) and
[docs/components.md](docs/components.md#nodes).

## Running it

```rust
let mut rt = RuntimeGraph::load(compiled, &artifacts, RuntimeConfig::default(), Perf).await?;
rt.inject(&"scale".into(), &"value".into(), Val::Float64(21.0)).await?;
if let TickResult::Faulted(report) = rt.tick().await {
    return Err(report.fault.into());
}
let out = rt.read_output(&"scale".into(), &"out".into()).await?;
```

`artifacts` maps each component to its bytes and WIT. `witgraph-source`
resolves a graph's component table into them, from files or OCI registries. A `tick` runs every node that has new input, and
repeats until nothing more can run. Nodes that need host features (a clock, a
key-value store) get them through capability plugins. See
[docs/running.md](docs/running.md).

A graph runs as one composed component: a generated hub holds its values and moves
them between nodes, so they don't pass through the host, which reads one only when
it asks. That makes it fast, and makes the graph one fault domain: a trap stops all
of it.

The same scheduler runs in a browser or in Node.js through `witgraph-web` and
`packages/witgraph-loader`, with the same calls: both hosts implement
`RunningGraph` from `witgraph-traits`. See [docs/javascript.md](docs/javascript.md).

## Editor

`packages/witgraph-editor` is a visual editor built as Web Components. Drag
components onto a canvas, connect ports, then run the graph in the browser and
watch each node's state and values. It supports breakpoints, stepping, mock
nodes, snapshots, undo and redo, and JSON import and export. See
[docs/editor.md](docs/editor.md); the demo app is in
`packages/witgraph-editor-demo`.

## Crates

| Crate                           | What it does                                                                      |
| ------------------------------- | --------------------------------------------------------------------------------- |
| `witgraph-ir`                   | Graph types, validation, and the `GraphBuilder → Graph → CompiledGraph` chain.    |
| `witgraph-wit`                  | Reads WIT into contracts and the editor catalog, and generates a graph's hub.     |
| `witgraph-traits`               | What the hosts have in common: the API of a running graph, and what runs one.     |
| `witgraph-sched`                | The scheduler. It holds no values, has no engine in it, and builds for `wasm32`.  |
| `witgraph-runtime`              | The scheduler on wasmtime.                                                        |
| `witgraph-web`                  | The scheduler for JavaScript hosts, via `wasm-bindgen`.                           |
| `witgraph-probes`               | Reads a guest's lines and variables from its DWARF, and puts probes on its lines. |
| `witgraph-source`               | Resolves nodes from files and OCI node artifacts, by profile; pushes them.        |
| `packages/witgraph-loader`      | TypeScript loader that runs a graph in a browser or Node.                         |
| `packages/witgraph-editor`      | Web Components for editing, running and debugging graphs visually.                |
| `packages/witgraph-editor-demo` | Demo app for the editor.                                                          |
| `test-fixtures/test-components` | Guest components for the tests. Not published.                                    |

## Building

```sh
cargo nextest run --workspace
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
```

`rust-toolchain.toml` pins the toolchain and the `wasm32-unknown-unknown` target,
so rustup installs both the first time you run cargo here.

For the TypeScript packages, run `pnpm install` once, then `pnpm check` to lint
and check formatting with [Biome](https://biomejs.dev), or `pnpm format` to fix
formatting.

wasm-tools, wac and jco come from the `witgraph-patches` branches of our forks,
which fix the upstream bugs witgraph hits. `pnpm install` builds jco's transpiler
from its fork, once per commit: that takes a few minutes and needs Rust.

`test-components` builds the guests listed in `GUESTS` in
`test-fixtures/test-components/build.rs`; a new directory under `guests/` is
not picked up until it is added there. Where a guest imports a standard WASI
interface, its `wit/deps` holds a copy of the upstream package: `just fetch-wit`
downloads the latest of each and moves the guests' imports to its version.

## Docs

- [Components](docs/components.md): writing a node.
- [Graphs](docs/graphs.md): connecting nodes and compiling a graph.
- [Running a graph](docs/running.md): loading, ticking, faults, snapshots.
- [JavaScript](docs/javascript.md): running graphs in a browser or Node.
- [Editor](docs/editor.md): the visual graph editor.

## License

BSD-3-Clause. See [LICENSE](LICENSE).
