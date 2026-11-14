# Graph editor

`witgraph-editor` is a visual editor for witgraph graphs, built as Web
Components. You draw the graph, run it, and watch it work, all in the browser.
It has no framework dependency, and it runs your real components through
[the loader](javascript.md), so what you see is what the scheduler does.

The demo in `packages/witgraph-editor-demo` shows everything described here.

![The demo graph running: each node previews its output as it changes, from
a feedback loop's values and a future's result to the frames of a video
stream through grayscale and invert filters.](assets/editor-demo.gif)

## The layout

| Element                | What it does                                                                                                                            |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `<witgraph-canvas>`    | The graph itself: nodes, ports and connections, drawn as SVG. Pan, zoom, drag nodes, and drag from one port to another to connect them. |
| `<witgraph-catalog>`   | The components available to add. Drag one onto the canvas.                                                                              |
| `<witgraph-inspector>` | Details for whatever is selected: a node, a connection, or the graph.                                                                   |
| `<witgraph-controls>`  | The toolbar: run, step, stop, reset, tick rate, breakpoints, undo and redo, layout, snapshots, import and export.                       |
| `<witgraph-timeline>`  | A trace of what ran, when, and what faulted.                                                                                            |
| `<witgraph-source>`    | The source of guests with debug info: set breakpoints in it and step through.                                                           |

Put them anywhere on your page. They all share one `EditorController`.

## Building a graph

- **Add** a node by dragging a component from the catalog.
- **Connect** ports by dragging from an output to an input. Ports are coloured
  by kind (value, stream or future), and the editor refuses connections the
  rules of a load don't allow (`controller.canConnect` tells you why).
- **See what's wrong.** A graph that doesn't compile marks the nodes and connections
  at fault in `--wg-error`, and what compiling warns of in `--wg-warning`, and the
  inspector says why. `controller.diagnosticsForNode` and
  `controller.diagnosticsForConnection` give you the same as data. A warning whose
  advice has been taken, like injecting a value into an input fed only by feedback,
  stays in the inspector marked as settled, and off the canvas
  (`controller.isSettled`).
- **Feedback.** Right-click a connection and choose _Mark as Feedback_ to make
  it a feedback edge, which cycles need.
- **Rename and label** nodes, and set their attributes, in the inspector.
- **Lay out** the graph automatically with _Layout_.
- **Edit freely.** Copy, cut, paste, delete, undo and redo all work, with the
  usual shortcuts (Ctrl+C, X, V, Z, Y and Delete). Right-click the canvas, a
  node or a connection for a menu.

## Running and debugging

- **Run** (F5) ticks until the graph settles or reaches a breakpoint, so a graph with a
  feedback loop or an endless stream runs until you stop it. On a graph that has finished,
  it runs it again.
- **Next node** (F10) runs one node and pauses before the next, which is selected, with its
  inputs in the inspector, shown as outputs are. It works from a stopped graph, a pause
  or a breakpoint, and past the last node it starts over. A node that is left running, reading
  an endless stream say, goes on beside what you step through.
- **Pause** (F6) holds each node from the next on: what is already running, streams
  included, goes on. **Continue** (F5) goes on to the next breakpoint, of either kind below.
- **Stop** (Shift+F5) halts a run and everything it left running, endless streams included.
  **Restart** (Ctrl+Shift+F5) stops and runs the graph again from the top, keeping its
  inputs, and **Reset** reloads it from scratch, emptied.
- **Editing waits for the run.** While a graph runs, or is paused in a run,
  what loads it again is refused: adding, removing, connecting, renaming,
  pasting, undoing and redoing, mock mode, importing and **Reset**. The
  toolbar and inspector disable them, and `controller.editable` tells a host
  of your own; an edit tried anyway does nothing but add a warning to the
  timeline. Stop the run first. Moving, resizing and labelling nodes, the tick
  rate, injected values and breakpoints go on working.
- **Rate** sets the most ticks a second a run makes (`controller.tickRate`),
  from one a second to _Max_, a tick a frame. Slow it down to watch a graph
  work; it can be changed while running.
- A tick lasts at most a tenth of a second, or one interval of the rate if
  that is shorter: work still running then (an endless stream, say) carries
  on into the next tick, so the rest of the graph keeps advancing beside it.
- Each node shows its state: pending, running, idle, faulted or cancelled, and a
  node in a stream group stays running until the group's streams have ended. A
  fault stops the whole graph, so every node shows it, with its cause and, when it
  is known, which node caused it. The next change or run starts every node again.
- **Breakpoints.** Toggle one on a node (toolbar _BP_ button, right-click, or F9).
  The run pauses before that node's next run.
- **Two levels.** The toolbar keeps them apart, and each button always does the same thing:

  | Level                                   | Moves with                                          | Breaks with                   | Stopped, it says   |
  | --------------------------------------- | --------------------------------------------------- | ----------------------------- | ------------------ |
  | **Graph**: whole nodes                  | **Next node**                                       | **Break on node** (F9)        | _Paused at a node_ |
  | **Code**: lines of a guest, if debugged | **Over** (F10), **Into** (F11), **Out** (Shift+F11) | A click in the panel's gutter | _Paused on a line_ |

  A click in the gutter sets a breakpoint, as in any debugger: every node running the line
  stops there. A click on it takes it away. Its right-click menu narrows it to the node on show,
  so that another node of its component runs past, or widens it to every node again. A
  breakpoint for every node is a dot, one for the node on show a ringed dot, and one for another
  node a faint ring, which this node runs past. Where a guest stops is said with its file, in the
  panel and in the status.

  They go together. **Next node** from a line finishes that node and pauses before the next, and
  **Into** from a node about to run stops at its first line. F10 does what is next at the level it
  is stopped at. **Pause** stops at whichever comes first, and **Continue** at the next of either.
- **Stopping in a stream.** A node's `run` starts its stream's work and often ends long
  before it does, and it runs again only when its inputs change once the stream has ended.
  So _Break on node_ stops a stream node once per stream, however many items pass. To stop
  for each item, click the gutter on a line of the loop that handles them.
- **Source lines.** Off unless you ask for it, as it **slows guests down significantly**. Load with
  `lines: true`, or `lines: { include, exclude, anchor, known }` to say what counts as the guest's
  own ([which lines are a guest's own](running.md#which-lines-are-a-guests-own)), and
  a guest built with debug info can be stopped on a line of its own code, even a node that is
  spinning. The source panel shows the files of the selected node's guest, and none with no node
  selected. A stop selects its node, and the panel follows it from file to file, and lists the
  call stack, where choosing a caller shows the line it is waiting on, in whichever file, and the
  variables in scope, marking what changed since the last stop in that function. Selecting
  another node while stopped shows its files, unmarked, until you select the stopped one again. A caller's variables are out of reach. All of the graph stops with it, so
  previews wait until it goes on. It takes a page that is cross-origin isolated
  ([why](javascript.md#stopping-on-a-source-line)): elsewhere the graph runs as usual, and the
  timeline says so. Without debug info in a graph, the panel and the Code buttons stay hidden.
  In the demo, add `?debug` to the
  address to turn it on: the page then loads the guests as `setup` built them with debug info.
  Its _Source lines: Rust, C and Zig_ graph (`?graph=c-lines&debug`) has a node written in each,
  the Rust one feeding the C one, which feeds the Zig one, each one's variables read as its
  language writes them.
- **Values.** Select a node to see the current value of each output port.
- **Previews.** A node can show one of its outputs inside it, as it changes:
  see [Previews](#previews).
- **Inputs.** Each Value input has a row on its node, beside its port. One fed by a
  connection shows the value it is given. One fed by nothing has a box: type a value in it
  as WAVE text and press Enter, or leave the box, to inject it, and empty the box to
  clear it. A value it refuses marks the box, with why in its tooltip, and Escape puts
  back what is injected. An input fed only by feedback has a box too, for its value on
  the first tick. What is injected is kept as the node's `inject.<port>` attribute, and
  the inspector can inject it as well.
- **Resources.** In the inspector, a node's _Resources_ say how much of a named
  resource it claims, and with nothing selected, the graph's say how much there is of
  each and how much is claimed. A node waits to start until its claim is free, and a
  claim too large for the graph is shown on the node. See [Limits](running.md#limits).
- **Mock mode.** In the inspector, turn on _Mock Mode_ for a node to skip its
  WebAssembly and give each output a value (written as WAVE text) instead.
  This is useful for trying a graph before every component exists.
- **Snapshots.** _Save_ captures the graph's state and _Restore_ goes back to
  it.

## Attaching to a graph on wasmtime

`controller.attach({ url, certificateHash })` debugs a graph a program runs on wasmtime and
serves with `witgraph-remote` ([Attaching a debugger](running.md#attaching-a-debugger)), over
WebTransport: it pins the server's certificate by `certificateHash`, and the `token` is in the
`url`. In the demo, open `?attach=<url>&hash=<hex of the hash>`;
`cargo run -p witgraph-remote --example serve` runs a graph to try it on, and prints both.

The program ticks the graph. _Stop_ halts it: the tick under way ends, and the program's ticks
advance nothing, what was under way waiting where it is, until _Run_ lets it go on. A guest
stopped on a line is let go, to go on once it runs, and its breakpoints are set again then, as
on a graph of the page's own. The badge says _Attached_, or _Attached, stopped_. _Restart_,
_Reset_ and _Rate_ are hidden, and the graph cannot be edited, as they are the program's. A
program built from another version of witgraph may speak another version of the debug
protocol: the editor refuses to attach to it, and says to rebuild it. What the program fails to
do, a breakpoint it did not set say, the timeline says. The rest is as on a graph of the page's
own: the components panel lists the graph's components, the timeline shows what the scheduler
did (the latest thousand events, from before attaching too), and node breakpoints, _Pause_,
_Continue_, _Next node_, the source panel (its text comes from the program's machine), _Inject_
and rerunning a node work. Previews of streams between nodes show what they carried, polled
with the states, if the program taps them (`RuntimeConfig::stream_previews` in
witgraph-runtime): the latest 64 items of each, and how many in all. Previews of handles are not
shown. A debugger that goes
lets go of what it held and stopped, and its breakpoints, and lets the graph run if it had
stopped it, within ten seconds if it went without closing. One that attaches takes over: what was left held, stopped or set, by a page reloaded
say, is let go at once.

## Previews

A node shows one of its outputs in a region below its ports: the frames of a
video stream as they arrive, say. Which output, and how, is up to the
component: its description names a viewer for its outputs ([Previewing outputs](components.md#previewing-outputs)), and the
node previews the first one it names.

- **Choosing.** _Preview_ in the inspector picks another output, or none. An
  output its component names no viewer for is plotted when it carries
  numbers and shown as text otherwise.
- **Values as text.** A record shows its fields as rows, names in one column and values
  in the other, a nested one indented under its field; a variant shows as its case, a
  record's under it, or as `case(payload)` on a line; an absent option as `—`. The
  inspector shows outputs so, a stream's latest item too, and a node's input field the
  value it is fed on one line. Inject still takes WAVE text.
- **How often.** A preview reads a Value output only when it has a new value,
  and by default as soon as it does. Set `previewIntervalMs` on the controller
  to read one output at most that often during a run: it is always read once
  more as the run ends.
- **Resizing.** Drag a node's bottom-right corner to resize it; its preview
  takes the height. Undo works.
- **Every kind of port.** A Value output's preview keeps each new value it sees
  (the value so far while a tick runs), a stream's every item as its reader takes
  it, and a future's value once it resolves, showing it _pending_ while its
  node runs again. A handle output's preview is given the object itself, as
  its maker returned it: a viewer for it reads what it needs through the
  object's own methods, and must expect it to be disposed once the next one
  replaces it. The inspector shows a handle by the object's `toString`, or by
  its resource, and offers no inject box for a handle input.
- **Streams and futures of handles.** A handle stays in the worker, so a viewer is
  shown what a [`HandleView`](javascript.md#in-a-worker) of the same name read of it, or
  else its description. There's no history, since each would hold on to what it read.
  Only what is previewed or selected is read: on a GPU, reading every frame can halve the
  frame rate.
- **Viewers.** `image` draws RGBA frames and PNG, JPEG, GIF or WebP bytes;
  `series` plots recent numbers; `text` shows any value, a stream's recent
  items, or a pending future. Add your own with `registerViewer`; it is
  given an `OutputView` (`kind`, `latest`, `recent`, `count`, `pending`):

```ts
import { registerViewer } from 'witgraph-editor';

registerViewer({
  id: 'level',
  mount(host) {
    const bar = document.createElement('meter');
    host.append(bar);
    return {
      update: ({ latest }) => (bar.value = Number(latest ?? 0)),
      dispose: () => bar.remove(),
    };
  },
});
```

A viewer of handles has a second half in the worker, which reads what it draws:

```ts
// worker.ts
const tensorView = {
  keep: (handle) => (handle as Tensor).clone(),
  read: async (tensor) => ({ shape: tensor.shape(), bytes: await tensor.read(0n, size(tensor)) }),
};
serveSession(self, { capabilities, views: { tensor: tensorView } });

// the page
registerViewer({
  id: 'tensor',
  mount(host) {
    /* `latest` is what `read` returned. */
  },
});
```

The node's choice of output and its size are kept in its attributes
(`editor.preview`, `editor.w` and `editor.h`), so they are saved with the
graph. `controller.observe(node, port)` gives what a preview is shown, and
`controller.latestOutput(node, port)` just its latest value. The history kept
is bounded (120 values, about 8 MiB).

## Saving and loading

_Load_ opens a graph from a JSON file, and _JSON_ downloads the current one.
_Bundle_ downloads the graph together with its components (their descriptions,
WIT and bytes), so it can be
opened without anything else. Graph and node attributes (`attrs`) are kept in
the JSON.

## Embedding it

```ts
import { EditorController, registerAll } from 'witgraph-editor';

registerAll(); // defines the <witgraph-*> elements
const controller = new EditorController();

for (const name of ['canvas', 'catalog', 'inspector', 'controls', 'timeline']) {
  document.querySelector(`witgraph-${name}`).controller = controller;
}

const worker = () => new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
await controller.load({ graph, components, wasm, worker });
await controller.autoLayout();
```

`sources` gives the panel a file's text, by the path the debug info has for it. Without it, or
when it returns nothing, the panel lists only the lines with code. A file changed since the guest
was built is shown as it is now, so lines may not match up.

The graph runs in a worker `worker` makes, so a node that spins leaves the editor working.
**Stop** gives a worker two seconds to answer. One that doesn't is ended, and the graph loaded
in a new one on the inputs it was given, so its nodes' outputs and state are gone, and the
timeline says so. The worker calls `serveSession` from [the loader](javascript.md#in-a-worker),
with a `capabilities` callback for anything the graph imports, which is asked each time the
graph is instantiated. `load` takes the graph as JSON, the components and the
scheduler's `wasm`. The controller emits
`state-changed`, `trace-event`, `fault` and `output-changed` events, so you can
show your own status or react to edits. `output-changed` fires at most once a
frame while streams carry items.

The catalog lists each world a component provides, described by its world's
doc comment
([Describing a component](components.md#describing-a-component)).

## Styling

The editor ships no theme. All colours and radii come from `--wg-*` CSS custom
properties with no fallback, so define them on the page or a parent element:

- Colours: `--wg-bg`, `--wg-surface`, `--wg-surface-hover`, `--wg-overlay`,
  `--wg-text`, `--wg-text-dim`, `--wg-text-muted`, `--wg-primary`,
  `--wg-primary-hover`, `--wg-accent`, `--wg-success`, `--wg-warning`,
  `--wg-error`.
- Port kinds: `--wg-port-value`, `--wg-port-stream`, `--wg-port-future`,
  `--wg-port-handle`.
- Node states: `--wg-phase-pending`, `--wg-phase-running`, `--wg-phase-idle`,
  `--wg-phase-faulted`, `--wg-phase-cancelled`. A node with a warning is outlined in
  `--wg-warning`, so keep that apart from the phases.
- Shape: `--wg-node-radius`, `--wg-port-radius`, `--wg-btn-radius`,
  `--wg-input-radius`, `--wg-group-radius`, `--wg-toggle-radius`,
  `--wg-node-grad-top`, `--wg-node-grad-bottom`.

`packages/witgraph-editor-demo/src/styles.css` has a complete example palette.

## Running the demo

1. `cargo build -p test-components`
2. In `packages/witgraph-loader`: `pnpm build:wasm`
3. In `packages/witgraph-editor-demo`: run the `setup` script (it copies the
   test components in), then `dev` to start the app.

The demo opens a graph of every port kind, whose nodes preview a Value, a
stream of numbers, a stream of bytes, a future and a video stream. The
picker at the top right switches to another graph:

| Graph                   | What it shows                                                                                                                                          |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `?graph=video`          | Video pipelines alone: frames streamed through grayscale and invert filters.                                                                           |
| `?graph=webgpu`         | Edge detection on WebGPU: the `edge-detect-webgpu` component runs a small CNN with fixed weights as compute shaders of its own, through `wasi:webgpu`. |
| `?graph=webnn`          | The same on WebNN, the `edge-detect` component running it through `test:accel/cnn`.                                                                    |
| `?graph=tensors`        | Tensors on the CPU: an image made once, its edges found and inverted by the tensors' own operations, nodes passing only handles.                       |
| `?graph=tensors-webgpu` | The same with the tensors kept in WebGPU storage buffers, and their operations run as compute shaders.                                                 |
| `?graph=tensors-webnn`  | The same with the tensors kept as WebNN tensors, and their operations run as WebNN graphs.                                                             |
| `?graph=camera`         | The page's camera on the CPU: a stream of tensors through edge detection and inversion, with nodes passing only handles. Asks for the camera when run. |
| `?graph=camera-webgpu`  | The same on WebGPU, where a frame goes straight from the video to a storage buffer, without the page's memory.                                         |
| `?graph=camera-webnn`   | The same on WebNN, whose frames pass through a canvas.                                                                                                 |

What runs these networks is the browser's, so a browser without WebGPU or
WebNN still loads its graph: the edge node faults when run, saying the
accelerator is missing. So does the image node of a tensor graph whose
backend the browser lacks. Every component is in the catalog in every graph
(in the default graph, `test:accel/cnn` runs on the CPU).

The Playwright tests run with `test` in the same package, in two Chromium
projects: `chromium` as it ships, which has neither accelerator (the edge
graphs, and the tensor graphs but the CPU one, must fault, saying so), and
`chromium-accelerated`, started with the flags turning both on (WebGPU on the
SwiftShader software renderer, so no GPU is needed), where the edges must be
computed and match a CPU reference, and the tensors must stay on their
device. Both use Chromium's fake camera. SwiftShader can't take a video frame
directly, so there the camera graph's frames pass through a canvas too.

Playwright reuses a server already on its port, whichever checkout it serves.
To test a worktree while another checkout's dev server is running, set
`DEMO_PORT`.
