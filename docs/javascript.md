# Running in JavaScript

`witgraph-web` is the same scheduler running against JavaScript, and
`packages/witgraph-loader` wraps it. It works in a browser and in Node. Each node's component is transpiled with
jco and instantiated on its own. Ticks, feedback, faults and snapshots behave
as they do on wasmtime, and snapshots have the same format.

```ts
const session = await Session.load({
  graph, // the graph, as JSON
  components, // [{ id: 'demo:graph/sensor@0.1.0', bytes, wit? }]
  capabilities: (node, capability) =>
    capability.interface === 'demo:caps/clock@0.1.0' ? { now: () => 0n } : undefined,
  mocks: { sensor: () => ({ latest: { value: 1, timestamp: 0n } }) },
  beforeRun: async (node, inputs) => {
    /* a breakpoint: the run waits */
  },
  onStreamItem: ({ node, port, items }) => console.log(node, port, items),
});
await session.graph.inject('scale', 'factor', 10);
await session.tick();
await session.graph.readOutput('scale', 'out');
session.dispose();
```

## Node.js

Node runs the loader's TypeScript source directly. The one difference from a
browser is that the scheduler's WebAssembly module can't be fetched by URL, so
pass its bytes to `init` once before using it:

```ts
import { readFile } from 'node:fs/promises';
import { init, Session } from 'witgraph-loader';

await init(await readFile('pkg/witgraph_web_bg.wasm'));
const session = await Session.load({ graph, components });
```

`Session.load` calls `init` itself, so you only need to call it first if you
use `describeComponent` or `jsName` without a session. In a browser, bundle
the TypeScript and let `init` fetch the module from beside the script.

## What it gives you

- **Capabilities** are plain JavaScript objects, returned from the
  `capabilities` callback.
- **Handles.** A resource a handle port carries is a class in the
  capability's object (`{ Tensor }`), the same class for every node, so an
  object one node makes is one another accepts. A handle output's object is
  passed to its readers as it is, and `readOutput` returns it. Once nothing
  holds it, its `[Symbol.dispose]()` is called, if it has one: free what it
  holds there (`buffer.destroy()`), not when it is collected. jco also calls
  it when a guest drops an `own` it was given, so every `own` a method
  returns must be an object of its own (a `clone` returns a new object
  sharing the data). A viewer showing an output must expect its object to be
  disposed once the next one replaces it.
- **Debugging.** `beforeRun` and `afterRun` run around every node call and can
  pause it, which gives you breakpoints and stepping. `onStreamItem` sees
  stream items as they pass between nodes, a chunk at a time: `item` is the
  chunk as jco yields it, and `items` the items in it: a stream of numbers
  (`NUMERIC_TYPES`) comes in typed arrays, which are its items as they are;
  anything else is one item a chunk.
- **Warnings.** `session.warnings` lists what was wrong with the components
  but did not stop the load (a viewer hint naming an output the component
  does not have, say); each is also written to the console.
- **Streams and futures of handles.** A `stream<own<R>>` or `future<own<R>>`
  port has a `handle` too. `onStreamItem` and `onFutureValue` are given the
  object itself, which is its reader's, and may be disposed as soon as the
  reader is done: to keep it, take your own reference, through the resource's
  API, before returning. The loader disposes what no reader takes.
  In Chromium such a stream costs about 14 ms a frame through jco, whatever the
  frame's size and however many nodes it passes (measured up to 10), so expect
  at most about 70 frames a second, less whatever the work itself takes.
- **Futures.** `onFutureValue` sees what each future output resolves to. With
  it, a future is read as soon as its node's run returns (its reader still
  gets the value).
- **Contracts.** `session.contracts` has each node's ports. An output whose
  component previews it in its `witgraph-viewer` section has a `viewer`
  ([Previewing outputs](components.md#previewing-outputs)). A handle port
  has a `handle`, the resource it carries.
- **Simulation.** `mocks` replaces a node with a function.
- **Values** use jco's representation: records are objects with camelCase keys,
  64-bit integers are `BigInt`s, lists of numbers are typed arrays, and
  streams are async iterables. A Value a node returns is passed to its
  readers as it is, not copied or converted, and `readOutput` returns that
  same value: don't change it.
- **The same calls as on wasmtime.** `inject`, `injectWave`, `clearInput`,
  `readOutput`, `readOutputWave`, `readHandle`, `snapshot` and `restore` return
  promises, on both hosts, and `tick` too. A handle output is read with
  `readHandle`. Reads work while a tick runs. Writes reject while one does.
- **Reading only what's new.** `outputVersion` says which value of an output is
  latched: a number that moves on each time the node writes it, or a snapshot
  is restored. It costs nothing to ask, so a host previewing an output on its
  own schedule can call `readOutput` only when the number has moved. Both work
  while a tick runs.

- **Large values.** WAVE text of a value (`readOutputWave`, `snapshot`) is
  refused, with an error naming the value, when it would not fit the
  scheduler's memory. Should the scheduler's WebAssembly module trap anyway
  (run out of memory, say), `session.broken` says in which call, or that it
  was during a tick (which then fails), and every call after throws that:
  load the graph again in a new session. A trap breaks every session, as
  they share the module.

Call `session.dispose()` when you're done with a session. It closes the
streams the nodes still have open, so a node writing one in the background
stops instead of running on, and a generation the scheduler abandons (on a
cancel or shutdown) has its streams closed the same way.

## Differences from wasmtime

- No fuel or memory limits, so a node that spins blocks the thread.
- A generation ends when every `run` has returned, without waiting for
  background work such as a stream writer.
- Nodes are async, so the browser needs JSPI.

## Building

`pnpm build:wasm` builds the scheduler into the package (it needs
`wasm-bindgen-cli` and the `wasm32-unknown-unknown` target), `pnpm test` runs
the tests after `cargo build -p test-components`, and `pnpm typecheck`
checks the source. `pnpm build` also compiles the TypeScript into `dist/`, which
is what gets published.

`pnpm build:wasm64` builds the scheduler for a 64-bit memory instead
(`wasm64-unknown-unknown`), for graphs whose values, as WAVE text or in
snapshots, need more than a 32-bit memory holds. It is opt-in: it needs a
nightly toolchain with `rust-src` (the standard library is built from
source), and a host with the memory64 proposal (recent Chromium and Node,
for instance). The guests stay 32-bit: components have no 64-bit ABI
yet.

`pnpm build:wasm:addr2line` builds the scheduler with its `addr2line` feature:
a trap's fault message then lists the WebAssembly frames of its stack, the
innermost first, as far as the engine's stack limit goes. Without the feature
the message is the trap's alone. To turn a frame's offset into a source line,
build the guest with debug info, transpile it, and look the offset up in the
core module jco emits:

```sh
wasm-tools addr2line node.core.wasm 0x1d47
```

An optimized guest may resolve to a file with no line, so build it with
`opt-level = 0` for the exact one.
