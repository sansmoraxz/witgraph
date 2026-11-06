# Running in JavaScript

`witgraph-web` is the same scheduler running against JavaScript, and
`packages/witgraph-loader` wraps it. It works in a browser and in Node. The graph
is composed into one component with the same generated hub as on wasmtime, which
is transpiled with jco and instantiated once. Values move inside it, and reach
JavaScript only when you ask; streams and futures do too, unless you set
`onStreamItem` or `onFutureValue`. Ticks, feedback, faults and snapshots behave as
they do on wasmtime, and snapshots have the same format.

```ts
const session = await Session.load({
  graph, // the graph, as JSON
  components, // [{ id: 'demo:graph/sensor@0.1.0', bytes, wit? }]
  capabilities: (capability) =>
    capability.interface === 'demo:caps/clock@0.1.0' ? { now: () => 0n } : undefined,
  mocks: { sensor: () => ({ latest: { value: 1, timestamp: 0n } }) },
  beforeRun: async (node) => {
    /* a breakpoint: the run waits; `session.readInputs(node)` says what with */
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
  `capabilities` callback, each time the graph is instantiated. Nodes importing one
  name share one import, so one object serves them all; two nodes wanting different
  providers of one interface use labels.
- **Handles.** A resource a handle port carries is a class in the
  capability's object (`{ Tensor }`), the same class for every node, so an
  object one node makes is one another accepts. A handle output's object is
  lent to its readers as it is, and `readHandle` returns it. Once nothing
  holds it, or its instance is gone, its `[Symbol.dispose]()` is called, if it has
  one: free what it holds there (`buffer.destroy()`), not when it is collected. jco
  also calls
  it when a guest drops an `own` it was given, so every `own` a method
  returns must be an object of its own (a `clone` returns a new object
  sharing the data). A viewer showing an output must expect its object to be
  disposed once the next one replaces it.
- **Debugging.** `beforeRun` and `afterRun` run around every node call and can
  pause it, which gives you breakpoints and stepping. They are told only the node:
  `session.readInputs(node)` and `graph.readOutput` read its values, if you want
  them, so nothing crosses to the host unasked. `onStreamItem` sees
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
- **Streams and futures nothing reads** are dropped by the hub, so their writers'
  writes fail, and the hooks below don't see them.
- **Futures.** `onFutureValue` sees what each future output something reads
  resolves to. With it, a future is read as soon as its node's run returns (its
  reader still gets the value).
- **Contracts.** `session.contracts` has each node's ports. An output whose
  component previews it in its `witgraph-viewer` section has a `viewer`
  ([Previewing outputs](components.md#previewing-outputs)). A handle port
  has a `handle`, the resource it carries.
- **Simulation.** `mocks` replaces a node with a function: it is left out of the
  composite, which calls yours instead, with inputs and outputs in jco's
  representation (a value may also be WAVE text). Mocking another node composes the
  graph anew.
- **Values** use jco's representation: records are objects with camelCase keys,
  64-bit integers are `BigInt`s, lists of numbers are typed arrays, and
  streams are async iterables. A Value is read out of the hub when you ask, so each
  `readOutput` makes a new one.
- **The same calls as on wasmtime.** `inject`, `injectWave`, `clearInput`,
  `readOutput`, `readOutputWave`, `readHandle`, `snapshot` and `restore` return
  promises, on both hosts, and `tick` too. A handle output is read with
  `readHandle`. Reads work while a tick runs. Writes, `rerun` and `shutdown`
  made while one does wait for it to end, and are done in the order they were
  made, so a write made while paused at a breakpoint lands once you go on.
  `rerun` and `shutdown` return promises here, for that. On wasmtime, they and
  the writes take the graph mutably, so they come between ticks there anyway.
  A second tick rejects while one runs.
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

Call `session.dispose()` when you're done with a session. It ends the instance: its
calls reject, its streams and futures close, so a node writing in the background
stops, its next call of a host import fails, and every object it still holds is
disposed once. An instance the scheduler lets go of (on a fault, shutdown or restore)
goes the same way.

## In a worker

A node runs on the thread its session is on, so one that spins holds up everything else
there, and a page that is busy holds up the nodes. `serveSession` puts a session in a
worker, and `RemoteSession` is the same session seen from the page:

```ts
// worker.ts
import { serveSession } from 'witgraph-loader';

serveSession(self, {
  capabilities: (capability) =>
    capability.interface === 'demo:caps/clock@0.1.0' ? clock : undefined,
  views: { tensor: tensorView },
});
```

```ts
// the page
const worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
const session = await RemoteSession.load(worker, {
  graph,
  components,
  wasm,
  observed: [['camera', 'frames']],
  onHandle: ({ node, port, shown }) => draw(shown),
});
await session.graph.inject('scale', 'factor', 10);
await session.tick();
await session.graph.readOutput('scale', 'out');
session.dispose();
```

Functions and resources can't cross, so a few things differ from a `Session`:

- **Capabilities are provided in the worker**, and so has to be whatever they use. Most of
  the web platform is there (WebGPU, WebNN, timers, `fetch`). What only a page has, like
  opening a camera, it does for the worker: the demo's `data/camera-page.ts` sends it
  `VideoFrame`s.
- **Handles stay in the worker.** A `HandleView`, named for the viewer an output asks for,
  says what the page is shown of one: `keep` takes a reference as it passes, and `read`
  makes a value of it, which can be cloned. Reads run one at a time for each output, and
  what arrives meanwhile waits, the newest only, so a stream of handles is sampled as fast
  as it reads back. With no view, a handle is described by its `toString`, or its resource.
- **Hooks run on the page, and most do not wait.** `beforeRun` is told each run, but only
  those `holds` names wait for what it returns (`holdAt` changes them, from the next run to
  start), so a busy page does not hold up the rest. `afterRun` does not wait.
- **Only what is observed crosses.** `onStreamItem`, `onFutureValue` and `onHandle` are told
  of the outputs in `observed` (`observeOnly` changes them), as each item is cloned to get
  there.
- **`mocks` run on the page**, on inputs and outputs that can be cloned.
- **What needs no waiting is as last reported.** `nodeState`, `outputVersion`, `takeTrace`
  and `takeFaults` read what the worker last sent, which it does as things happen, at most
  every 16 ms. A tick's reply comes after the state as it ended, and `graph.sync()` waits for
  the state as it is. `onState` says when it has moved. `catchUp` returns at once, and
  stays in order with everything else asked of the session.

A value that can't be cloned rejects the call that asked for it. One worker serves any
number of sessions. `endWorker(worker)` is the only way to end a node that spins, since
the worker never gets to read a `shutdown`. It ends all of them, and what waits on them
rejects. A `MessagePort` will do
for `self` and `worker`, with `serveSession` on either side of it, even on one thread.

### Stopping on a source line

Debugging is off unless you ask for it, as **it slows a graph down significantly**. Load with
`lines`, and a guest built with debug info can be stopped on any line of its source. A browser
lets a page set no breakpoint in WebAssembly, so each line gets a probe as the graph loads, which
checks a flag and carries on. That can slow a tight loop by half, even with no breakpoint set, and
the unoptimized build it wants is slower still. Leave it off for anything but debugging: without
`lines`, guests run as they were built, with nothing added.

```ts
const session = await RemoteSession.load(worker, {
  graph,
  components,
  wasm,
  lines: { onStop: (stop) => show(stop.file, stop.line, stop.node, stop.frames) },
});
const { lines } = session;
// `file` from `lines.filesOf(node)`, or `lines.files` for every node's. It returns the line it
// came to. `{ node }` stops only that node; `'every-node'` each one whose guest has the line.
lines.setBreakpoint({ node: 'walker' }, file, 42);
// `stop.frames` is the stack: `{ function, file?, line? }`, the line a caller's call is on.
void session.tick();
// ... `onStop` says where it stopped, and what its variables hold; then one of:
lines.stepOver();
lines.stepIn();
lines.stepOut();
lines.resume();
```

`pause()` stops at the next line any guest reaches. It and `setBreakpoint` take effect at
once, even on a guest in a loop that never returns to its worker. A breakpoint on a line
with no code comes to the next that has some. Two nodes of one component run the same lines:
one set for a node stops it alone, and the other runs past at no cost. One set for a node and one
for every node are two, each cleared on its own. A step stays with its guest: others run on past
their probes.

- **The page must be cross-origin isolated**, for the shared memory this goes through: serve
  it with `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`. It must be a real worker too, as a stop blocks
  its thread.
- **Variables come with the stop**, in `stop.variables`: `{ name, type, value, children }`,
  with `value` on one line and `children` to look into, read as the guest stopped since it
  cannot be asked later. They are those in scope at that line of the innermost function: its
  callers' locals are out of reach from outside them. Rust's `String`, `Vec`, slices, enums and
  `Option` read as their source would write them, and so do C's structs, unions, arrays, enums,
  `char`s and `char *` strings (`{ .x = 3, .y = 2.5 }`, `const char *`), by the language the
  debug info names; anything else field by field. Text, lists
  and nesting are cut off past a limit. A function's parameters hold nonsense on its opening line, before it has stored them, so a step
  goes past that line, though a breakpoint set on it stops there. Optimizing leaves few variables to read: at `opt-level = 0` you get them all, and
  above that most show as `<optimized out>`, or are not listed at all.
- **Stopped, the worker answers nothing.** All its guests are stopped with it, as on wasmtime,
  and what you ask of the session waits, until `resume()` or a step. `dispose()` lets the guest
  go first, and `endWorker` ends it where it is.
- **Build the guest with debug info**, and `opt-level = 0` for exact lines: optimizing leaves
  some lines with no code of their own. A guest with none has no lines, and runs as usual.
- **Only the guest's own lines** have probes: not the standard library's, or a dependency's.
  Stepping stays out of those, and so does their cost. Which they are is decided as on wasmtime
  ([which lines are a guest's own](running.md#which-lines-are-a-guests-own)), by
  `lines: { include, exclude, anchor, known }`: by default, any language's, what is under the
  directory the guest's `run` was compiled in, less the libraries witgraph knows of.
  `lines: { exclude: ['**/bindings/**'] }` leaves out generated bindings, and
  `lines: { include: ['/home/me/project/**'], exclude: ['**'] }` takes only that directory.
  `lines: { all: true }` puts them on every line, at about twice the time. A step does not stop
  in a function all on one line, which is what a macro usually generates, though a breakpoint
  there does.

## Attaching to a graph on wasmtime

`AttachedSession.attach({ url, certificateHash, beforeRun, onStop, onState, onError })` is a session of a
graph a program runs on wasmtime, served with `witgraph-remote`
([Attaching a debugger](running.md#attaching-a-debugger)), over wRPC on WebTransport. It has
the shape of a worker's `RemoteSession` (`DebugSession`), so a debugger uses either the same
way, but the program ticks the graph, so `tick` rejects, as do `snapshot`, `restore` and
`shutdown`. A held run is told to `beforeRun(node, true)` and goes on once it settles; a stop on
a line to `onStop`. Node states and output versions are asked for every `pollEvery`
milliseconds (250), and `onState` says when they have moved. `source(file)` reads a guest's
source from the program's machine. `stop()` halts the program's graph, so that its ticks advance
nothing, and `run()` lets it go on; `running` says which, and `onState` says when it changes.
Attaching lets go of what another debugger left held, stopped or set, and what is asked is sent
in the order asked: what the program fails to do is told to `onError`. A program speaking
another version of the debug protocol than `PROTOCOL` is refused, with an error saying which to
rebuild.

Its calls are bindings `wit-bindgen-wrpc js` makes from `crates/witgraph-remote/wit`, over
`@bytecodealliance/wrpc`, both from our wRPC fork. After changing the WIT, generate them again:

```sh
wit-bindgen-wrpc js --world debugger --generate-all \
  --out-dir packages/witgraph-loader/src/attach/bindings crates/witgraph-remote/wit
```

## When a graph does not compile

`Session.load` throws an error listing what is wrong. `diagnosticsOf(error)` gets the
same as data: each diagnostic has a stable `code`, a `severity`, a `message`, perhaps
some `help`, and a `location` with the `nodes` it touches, so you can mark them.

A graph that compiles can still have warnings: `session.diagnostics` has them, in the
same form.

`GraphChecker` compiles graphs with nothing loaded, by the same rules, so you can ask
about a graph before you make it:

```ts
await init(wasm);
const checker = new GraphChecker(components);
const diagnostics = checker.check(JSON.stringify(graph));
```

## Differences from wasmtime

- No fuel or memory limits, so a node that spins blocks its thread. In a
  [worker](#in-a-worker) that thread is not the page's.
- What a failed call is pinned on is the node whose call failed.
- Nodes are async, so the browser needs JSPI.

## Transpiling

A composite must be transpiled with `multiMemory`, or jco copies each list in
JavaScript, about 300 times slower, with no error.

The loader uses `@bytecodealliance/jco-transpile` from the `witgraph-patches` branch
of a fork of jco, which fixes what witgraph hits. `pnpm install` builds it, once per
commit: that takes a few minutes and needs Rust.

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
