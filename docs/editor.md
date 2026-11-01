# Graph editor

`witgraph-editor` is a visual editor for witgraph graphs, built as Web
Components. You draw the graph, run it, and watch it work, all in the browser.
It has no framework dependency, and it runs your real components through
[the loader](javascript.md), so what you see is what the scheduler does.

The demo in `packages/witgraph-editor-demo` shows everything described here.

## The layout

| Element | What it does |
|---|---|
| `<witgraph-canvas>` | The graph itself: nodes, ports and connections, drawn as SVG. Pan, zoom, drag nodes, and drag from one port to another to connect them. |
| `<witgraph-catalog>` | The components available to add. Drag one onto the canvas. |
| `<witgraph-inspector>` | Details for whatever is selected: a node, a connection, or the graph. |
| `<witgraph-controls>` | The toolbar: run, step, stop, reset, breakpoints, undo and redo, layout, snapshots, import and export. |
| `<witgraph-timeline>` | A trace of what ran, when, and what faulted. |

Put them anywhere on your page. They all share one `EditorController`.

## Building a graph

- **Add** a node by dragging a component from the catalog.
- **Connect** ports by dragging from an output to an input. Ports are coloured
  by kind (value, stream or future), and the editor refuses connections the
  contracts don't allow (`controller.canConnect` tells you why).
- **Feedback.** Right-click a connection and choose *Mark as Feedback* to make
  it a feedback edge, which cycles need.
- **Rename and label** nodes, and set their attributes, in the inspector.
- **Lay out** the graph automatically with *Layout*.
- **Edit freely.** Copy, cut, paste, delete, undo and redo all work, with the
  usual shortcuts (Ctrl+C, X, V, Z, Y and Delete). Right-click the canvas, a
  node or a connection for a menu.

## Running and debugging

- **Run** ticks until the graph settles. **Step** runs a single tick. **Stop**
  halts a run, and **Reset** reloads the graph from scratch. A delay slider
  slows ticks down so you can watch them.
- Each node shows its state: pending, running, idle, faulted or cancelled. A
  fault shows its cause and, when it is known, which node caused it.
- **Breakpoints.** Toggle one on a node (toolbar *BP* button or right-click).
  The run pauses before that node, and *Resume* continues.
- **Values.** Select a node to see the current and past values of each output
  port. *Expand Chart* on a node draws a line chart of an output's value on each tick.
- **Inputs.** Give a node an `inject.<port>` attribute to feed that input a
  fixed value.
- **Mock mode.** In the inspector, turn on *Mock Mode* for a node to skip its
  WebAssembly and give each output a value (written as WAVE text) instead.
  This is useful for trying a graph before every component exists.
- **Snapshots.** *Save* captures the graph's state and *Restore* goes back to
  it.

## Saving and loading

*Load* opens a graph from a JSON file, and *JSON* downloads the current one.
*Bundle* downloads the graph together with its component bytes, so it can be
opened without anything else. Graph and node attributes (`attrs`) are kept in
the JSON.

## Embedding it

```ts
import { EditorController, registerAll } from 'witgraph-editor';

registerAll();                       // defines the <witgraph-*> elements
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
show your own status or react to edits.

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

The Playwright tests run with `test` in the same package.
