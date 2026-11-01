# Running in JavaScript

`witgraph-web` is the same scheduler running against JavaScript, and
`packages/witgraph-loader` wraps it. It works in a browser and in Node. Each node's component is transpiled with
jco and instantiated on its own. Ticks, feedback, faults and snapshots behave
as they do on wasmtime, and snapshots have the same format.

```ts
const session = await Session.load({
  graph,                       // the graph, as JSON
  components,                  // [{ id: 'demo:graph/sensor@0.1.0', bytes, wit? }]
  capabilities: (node, capability) =>
    capability.interface === 'demo:caps/clock@0.1.0' ? { now: () => 0n } : undefined,
  mocks: { sensor: () => ({ latest: { value: 1, timestamp: 0n } }) },
  beforeRun: async (node, inputs) => { /* a breakpoint: the run waits */ },
  onStreamItem: ({ node, port, item }) => console.log(node, port, item),
});
session.graph.inject('scale', 'factor', 10);
await session.tick();
session.graph.readOutput('scale', 'out');
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
- **Debugging.** `beforeRun` and `afterRun` run around every node call and can
  pause it, which gives you breakpoints and stepping. `onStreamItem` sees
  stream items as they pass between nodes.
- **Simulation.** `mocks` replaces a node with a function.
- **Values** use jco's representation: records are objects with camelCase keys,
  64-bit integers are `BigInt`s, and streams are async iterables.

Call `session.dispose()` when you're done with a session.

## Differences from wasmtime

- No fuel or memory limits, so a node that spins blocks the thread.
- A generation ends when every `run` has returned, without waiting for
  background work such as a stream writer.
- Links aren't supported yet.
- Nodes are async, so the browser needs JSPI.

## Building

`npm run build:wasm` builds the scheduler into the package (it needs
`wasm-bindgen-cli` and the `wasm32-unknown-unknown` target), `npm test` runs
the tests after `cargo build -p test-components`, and `npm run typecheck`
checks the source. The package is private and isn't published.
