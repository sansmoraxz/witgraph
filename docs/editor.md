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

Put them anywhere on your page. They all share one `EditorController`.

## Building a graph

- **Add** a node by dragging a component from the catalog.
- **Connect** ports by dragging from an output to an input. Ports are coloured
  by kind (value, stream or future), and the editor refuses connections the
  contracts don't allow (`controller.canConnect` tells you why).
- **Feedback.** Right-click a connection and choose _Mark as Feedback_ to make
  it a feedback edge, which cycles need.
- **Rename and label** nodes, and set their attributes, in the inspector.
- **Lay out** the graph automatically with _Layout_.
- **Edit freely.** Copy, cut, paste, delete, undo and redo all work, with the
  usual shortcuts (Ctrl+C, X, V, Z, Y and Delete). Right-click the canvas, a
  node or a connection for a menu.

## Running and debugging

- **Run** ticks until the graph settles, so a graph with a feedback loop or
  an endless stream runs until you stop it. **Step** runs a single tick.
  **Stop** halts a run and everything it left running, endless streams
  included, and **Reset** reloads the graph from scratch.
- **Rate** sets the most ticks a second a run makes (`controller.tickRate`),
  from one a second to _Max_, a tick a frame. Slow it down to watch a graph
  work; it can be changed while running.
- A tick lasts at most a tenth of a second, or one interval of the rate if
  that is shorter: work still running then (an endless stream, say) carries
  on into the next tick, so the rest of the graph keeps advancing beside it.
- Each node shows its state: pending, running, idle, faulted or cancelled. A
  fault shows its cause and, when it is known, which node caused it.
- **Breakpoints.** Toggle one on a node (toolbar _BP_ button or right-click).
  The run pauses before that node, and _Resume_ continues.
- **Values.** Select a node to see the current value of each output port.
- **Previews.** A node can show one of its outputs inside it, as it changes:
  see [Previews](#previews).
- **Inputs.** Give a node an `inject.<port>` attribute to feed that input a
  fixed value.
- **Mock mode.** In the inspector, turn on _Mock Mode_ for a node to skip its
  WebAssembly and give each output a value (written as WAVE text) instead.
  This is useful for trying a graph before every component exists.
- **Snapshots.** _Save_ captures the graph's state and _Restore_ goes back to
  it.

## Previews

A node shows one of its outputs in a region below its ports: the frames of a
video stream as they arrive, say. Which output, and how, is up to the
component: its `witgraph-viewer` custom section names a viewer for its
outputs ([Previewing outputs](components.md#previewing-outputs)), and the
node previews the first one it names.

- **Choosing.** _Preview_ in the inspector picks another output, or none. An
  output its component names no viewer for is plotted when it carries
  numbers and shown as text otherwise.
- **Resizing.** Drag a node's bottom-right corner to resize it; its preview
  takes the height. Undo works.
- **Every kind of port.** A Value output's preview keeps a value a tick (the
  value so far while a tick runs), a stream's every item as its reader takes
  it, and a future's value once it resolves, showing it _pending_ while its
  node runs again.
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

The node's choice of output and its size are kept in its attributes
(`editor.preview`, `editor.w` and `editor.h`), so they are saved with the
graph. `controller.observe(node, port)` gives what a preview is shown, and
`controller.latestOutput(node, port)` just its latest value. The history kept
is bounded (120 values, about 8 MiB).

## Saving and loading

_Load_ opens a graph from a JSON file, and _JSON_ downloads the current one.
_Bundle_ downloads the graph together with its component bytes, so it can be
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

await controller.load({ graph, components, wasm, capabilities });
await controller.autoLayout();
```

`load` takes the same things as `Session.load` in [the loader](javascript.md):
the graph as JSON, the component bytes, the scheduler's `wasm`, and a
`capabilities` callback for anything the nodes import. The controller emits
`state-changed`, `trace-event`, `fault` and `output-changed` events, so you can
show your own status or react to edits. `output-changed` fires at most once a
frame while streams carry items.

## Styling

The editor ships no theme. All colours and radii come from `--wg-*` CSS custom
properties with no fallback, so define them on the page or a parent element:

- Colours: `--wg-bg`, `--wg-surface`, `--wg-surface-hover`, `--wg-overlay`,
  `--wg-text`, `--wg-text-dim`, `--wg-text-muted`, `--wg-primary`,
  `--wg-primary-hover`, `--wg-accent`, `--wg-success`, `--wg-warning`,
  `--wg-error`.
- Port kinds: `--wg-port-value`, `--wg-port-stream`, `--wg-port-future`.
- Node states: `--wg-phase-pending`, `--wg-phase-running`, `--wg-phase-idle`,
  `--wg-phase-faulted`, `--wg-phase-cancelled`.
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

| Graph           | What it shows                                                                                                                             |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `?graph=video`  | Video pipelines alone: frames streamed through grayscale and invert filters.                                                              |
| `?graph=webgpu` | Edge detection on WebGPU: the `edge-detect` component runs a small CNN with fixed weights through `test:accel/cnn`, here compute shaders. |
| `?graph=webnn`  | The same on WebNN.                                                                                                                        |

What runs `test:accel/cnn` is the browser's, so a browser without WebGPU or
WebNN still loads its graph: the edge node faults when run, saying the
accelerator is missing. Every component is in the catalog in every graph
(in the default graph, `test:accel/cnn` runs on the CPU).

The Playwright tests run with `test` in the same package, in two Chromium
projects: `chromium` as it ships, which has neither accelerator (the edge
graphs must fault, saying so), and `chromium-accelerated`, started with the
flags turning both on (WebGPU on the SwiftShader software renderer, so no
GPU is needed), where the edges must be computed and match a CPU
reference.
