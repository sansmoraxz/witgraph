/**
 * WIT Graph Editor — Demo
 *
 * Showcases all port kinds (Value, Stream, Future), external capabilities,
 * and mock support — running real WASM components via jco. `?graph=video`
 * loads a video pipeline instead, each node previewing the frames it emits.
 *
 * Prerequisites:
 *   cargo build -p test-components
 *   cd packages/witgraph-loader && pnpm build:wasm
 *   cd packages/witgraph-editor-demo && pnpm setup && pnpm dev
 *
 * (setup.mjs is cross-platform — no bash required)
 */

import {
  EditorController,
  type GraphJson,
  registerAll,
  registerViewer,
  type WitgraphCanvas,
  type WitgraphCatalog,
  type WitgraphControls,
  type WitgraphInspector,
  type WitgraphSource,
  type WitgraphTimeline,
} from 'witgraph-editor';
import type { ComponentSource } from 'witgraph-loader';
import { serveCameraFrames } from './data/camera-page.ts';
import { tensorViewer } from './data/viewer.ts';

registerAll();
registerViewer(tensorViewer);

const controller = new EditorController();

const canvas = document.querySelector('witgraph-canvas') as WitgraphCanvas;
const controls = document.querySelector('witgraph-controls') as WitgraphControls;
const inspector = document.querySelector('witgraph-inspector') as WitgraphInspector;
const timeline = document.querySelector('witgraph-timeline') as WitgraphTimeline;
const source = document.querySelector('witgraph-source') as WitgraphSource;
const catalog = document.querySelector('witgraph-catalog') as WitgraphCatalog;
const statusEl = document.getElementById('status')!;

canvas.controller = controller;
controls.controller = controller;
inspector.controller = controller;
timeline.controller = controller;
source.controller = controller;
catalog.controller = controller;

// -- Component references -----------------------------------------------------

const relayRef = {
  package: { namespace: 'test', name: 'relay', version: '0.1.0' },
  world: 'relay',
};

const streamProducerRef = {
  package: { namespace: 'test', name: 'stream-producer', version: '0.1.0' },
  world: 'stream-producer',
};

const streamConsumerRef = {
  package: { namespace: 'test', name: 'stream-consumer', version: '0.1.0' },
  world: 'stream-consumer',
};

const futureWriterRef = {
  package: { namespace: 'test', name: 'future-writer', version: '0.1.0' },
  world: 'future-writer',
};

const futureReaderRef = {
  package: { namespace: 'test', name: 'future-reader', version: '0.1.0' },
  world: 'future-reader',
};

const configurableRef = {
  package: { namespace: 'test', name: 'configurable', version: '0.1.0' },
  world: 'configurable',
};

const calcRef = {
  package: { namespace: 'test', name: 'calc', version: '0.1.0' },
  world: 'calc',
};

const mqttNodeRef = {
  package: { namespace: 'test', name: 'mqtt-node', version: '0.1.0' },
  world: 'mqtt-node',
};

const bytesConsumerRef = {
  package: { namespace: 'test', name: 'bytes-consumer', version: '0.1.0' },
  world: 'bytes-consumer',
};

const videoSourceRef = {
  package: { namespace: 'test', name: 'video-source', version: '0.1.0' },
  world: 'video-source',
};

const frameGrayscaleRef = {
  package: { namespace: 'test', name: 'frame-grayscale', version: '0.1.0' },
  world: 'frame-grayscale',
};

const frameInvertRef = {
  package: { namespace: 'test', name: 'frame-invert', version: '0.1.0' },
  world: 'frame-invert',
};

const videoSinkRef = {
  package: { namespace: 'test', name: 'video-sink', version: '0.1.0' },
  world: 'video-sink',
};

// -- Graph definitions --------------------------------------------------------

/**
 * Every component, in the order of every graph's component table: each
 * graph has them all, so the catalog offers them all whichever is loaded.
 */
const GUESTS = [
  'busy-loop',
  'line-walk',
  'relay',
  'stream-producer',
  'stream-consumer',
  'future-writer',
  'future-reader',
  'configurable',
  'calc',
  'mqtt-node',
  'bytes-consumer',
  'video-source',
  'frame-grayscale',
  'frame-invert',
  'video-sink',
  'edge-detect',
  'edge-detect-webgpu',
  'tensor-source',
  'tensor-edge',
  'tensor-invert',
  'tensor-sink',
  'tensor-stream-invert',
  'tensor-stream-edge',
  'tensor-stream-sink',
  'camera-source',
];

const allComponents = GUESTS.map((guest) => ({
  package: { namespace: 'test', name: guest, version: '0.1.0' },
  world: guest,
}));

const lineWalkRef = {
  package: { namespace: 'test', name: 'line-walk', version: '0.1.0' },
  world: 'line-walk',
};

const portKinds: GraphJson = {
  metadata: {
    name: 'demo-all-port-kinds',
    description:
      'Showcasing Value, Stream, Future, external capabilities, mock support and in-node previews with real WASM components.',
  },
  components: allComponents,
  nodes: [
    // Value chain
    {
      id: 'relay-a',
      component: relayRef,
      label: 'Relay A',
      attrs: { 'inject.in': '0', 'inject.add': '1' },
    },
    { id: 'relay-b', component: relayRef, label: 'Relay B', attrs: { 'inject.add': '0' } },
    { id: 'relay-c', component: relayRef, label: 'Relay C', attrs: { 'inject.add': '0' } },
    // Stream group
    {
      id: 'producer',
      component: streamProducerRef,
      label: 'Producer',
      attrs: { 'inject.burst-size': '5' },
    },
    { id: 'consumer', component: streamConsumerRef, label: 'Consumer' },
    // Future group
    { id: 'writer', component: futureWriterRef, label: 'Future Writer' },
    { id: 'reader', component: futureReaderRef, label: 'Future Reader' },
    // Display relay
    { id: 'display', component: relayRef, label: 'Display', attrs: { 'inject.add': '0' } },
    // Configurable node (pure — no external capabilities)
    {
      id: 'configurable',
      component: configurableRef,
      label: 'Configurable',
      attrs: { 'inject.rate': '3', 'inject.gain': '2.5' },
    },
    // Calc node (imports test:math/ops — satisfied by JS capabilities callback)
    { id: 'calc', component: calcRef, label: 'Calc (ext caps)', attrs: { 'inject.x': '5' } },
    // MQTT node (imports test:mqtt/source — satisfied by JS capabilities callback)
    { id: 'mqtt', component: mqttNodeRef, label: 'MQTT Sensor' },
    // Bytes consumer (receives mqtt stream)
    { id: 'mqtt-sink', component: bytesConsumerRef, label: 'Msg Counter' },
    // Video group: frames streamed through a filter, each node previewing them
    {
      id: 'camera',
      component: videoSourceRef,
      label: 'Camera',
      attrs: { 'inject.fps': '20' },
    },
    { id: 'grayscale', component: frameGrayscaleRef, label: 'Grayscale' },
    { id: 'invert', component: frameInvertRef, label: 'Invert' },
    { id: 'video-sink', component: videoSinkRef, label: 'Frame Sink' },
  ],
  connections: [
    {
      id: 'a-feedback',
      from: { node: 'relay-a', port: 'out' },
      to: { node: 'relay-a', port: 'in' },
      feedback: true,
    },
    { id: 'a-to-b', from: { node: 'relay-a', port: 'out' }, to: { node: 'relay-b', port: 'in' } },
    { id: 'b-to-c', from: { node: 'relay-b', port: 'out' }, to: { node: 'relay-c', port: 'in' } },
    {
      id: 'stream',
      from: { node: 'producer', port: 'items' },
      to: { node: 'consumer', port: 'items' },
    },
    {
      id: 'future-link',
      from: { node: 'writer', port: 'done' },
      to: { node: 'reader', port: 'data' },
    },
    {
      id: 'writer-to-display',
      from: { node: 'writer', port: 'last' },
      to: { node: 'display', port: 'in' },
    },
    {
      id: 'mqtt-to-sink',
      from: { node: 'mqtt', port: 'messages' },
      to: { node: 'mqtt-sink', port: 'items' },
    },
    {
      id: 'camera-to-grayscale',
      from: { node: 'camera', port: 'frames' },
      to: { node: 'grayscale', port: 'frames' },
    },
    {
      id: 'grayscale-to-invert',
      from: { node: 'grayscale', port: 'frames' },
      to: { node: 'invert', port: 'frames' },
    },
    {
      id: 'invert-to-sink',
      from: { node: 'invert', port: 'frames' },
      to: { node: 'video-sink', port: 'frames' },
    },
  ],
};

/**
 * Two cameras streaming frames through image filters. Each component names
 * the viewer for its `frames` output in a `witgraph-viewer` custom section,
 * so every node but the sinks previews its frames as they stream.
 */
const video: GraphJson = {
  metadata: {
    name: 'demo-video-pipeline',
    description: 'Frames streamed from a camera through grayscale and invert filters.',
  },
  components: allComponents,
  nodes: [
    {
      id: 'camera',
      component: videoSourceRef,
      label: 'Camera',
      attrs: { 'inject.fps': '15' },
    },
    { id: 'gray', component: frameGrayscaleRef, label: 'Grayscale' },
    { id: 'gray-invert', component: frameInvertRef, label: 'Invert' },
    { id: 'gray-sink', component: videoSinkRef, label: 'Sink' },
    {
      id: 'camera-b',
      component: videoSourceRef,
      label: 'Camera B',
      attrs: { 'inject.fps': '10' },
    },
    { id: 'invert', component: frameInvertRef, label: 'Invert' },
    { id: 'sink', component: videoSinkRef, label: 'Sink' },
  ],
  connections: [
    {
      id: 'camera-gray',
      from: { node: 'camera', port: 'frames' },
      to: { node: 'gray', port: 'frames' },
    },
    {
      id: 'gray-invert',
      from: { node: 'gray', port: 'frames' },
      to: { node: 'gray-invert', port: 'frames' },
    },
    {
      id: 'gray-invert-sink',
      from: { node: 'gray-invert', port: 'frames' },
      to: { node: 'gray-sink', port: 'frames' },
    },
    {
      id: 'camera-b-invert',
      from: { node: 'camera-b', port: 'frames' },
      to: { node: 'invert', port: 'frames' },
    },
    {
      id: 'invert-sink',
      from: { node: 'invert', port: 'frames' },
      to: { node: 'sink', port: 'frames' },
    },
  ],
};

/** The edge node of an accelerator: WebGPU's runs its own shaders on it. */
const edgeDetectRef = (accelerator: string) => {
  const name = accelerator === 'WebGPU' ? 'edge-detect-webgpu' : 'edge-detect';
  return { package: { namespace: 'test', name, version: '0.1.0' }, world: name };
};

/**
 * A camera's frames through edge detection: a small convolutional network
 * with fixed weights, run on the browser's accelerator: compute shaders of
 * the node's own through `wasi:webgpu`, or WebNN behind `test:accel/cnn`. A
 * browser without the accelerator still loads the graph; the edge node faults
 * when run, saying so.
 */
function edgeGraph(accelerator: string): GraphJson {
  return {
    metadata: {
      name: `demo-edges-${accelerator.toLowerCase()}`,
      description: `Edge detection by a fixed-weight CNN on ${accelerator}.`,
    },
    components: allComponents,
    nodes: [
      { id: 'camera', component: videoSourceRef, label: 'Camera', attrs: { 'inject.fps': '15' } },
      { id: 'edges', component: edgeDetectRef(accelerator), label: `Edges (${accelerator})` },
      { id: 'sink', component: videoSinkRef, label: 'Sink' },
    ],
    connections: [
      {
        id: 'camera-edges',
        from: { node: 'camera', port: 'frames' },
        to: { node: 'edges', port: 'frames' },
      },
      {
        id: 'edges-sink',
        from: { node: 'edges', port: 'edges' },
        to: { node: 'sink', port: 'frames' },
      },
    ],
  };
}

const tensorRef = (name: string) => ({
  package: { namespace: 'test', name, version: '0.1.0' },
  world: name,
});

/**
 * Tensors that stay where `where` keeps them: an image made once, its
 * edges found and inverted there, and the result summed. Nodes pass only
 * handles; previews and the sink read the data back.
 */
function tensorGraph(where: string): GraphJson {
  return {
    metadata: {
      name: `demo-tensors-${where.toLowerCase()}`,
      description: `Tensors on ${where}: nodes pass handles, operations run where the data lives.`,
    },
    components: allComponents,
    nodes: [
      { id: 'image', component: tensorRef('tensor-source'), label: 'Image' },
      { id: 'edges', component: tensorRef('tensor-edge'), label: `Edges (${where})` },
      { id: 'invert', component: tensorRef('tensor-invert'), label: 'Invert' },
      { id: 'sum', component: tensorRef('tensor-sink'), label: 'Sum' },
    ],
    connections: [
      {
        id: 'image-edges',
        from: { node: 'image', port: 'tensor' },
        to: { node: 'edges', port: 'tensor' },
      },
      {
        id: 'edges-invert',
        from: { node: 'edges', port: 'edges' },
        to: { node: 'invert', port: 'tensor' },
      },
      {
        id: 'invert-sum',
        from: { node: 'invert', port: 'tensor' },
        to: { node: 'sum', port: 'tensor' },
      },
    ],
  };
}

/**
 * The page's camera through edge detection and inversion, on `where`: a
 * stream of tensors. Nodes pass only handles, and a preview samples what it
 * can read back. The sink reads nothing, so nothing else leaves the device.
 */
function cameraGraph(where: string): GraphJson {
  return {
    metadata: {
      name: `demo-camera-${where.toLowerCase()}`,
      description: `The camera on ${where}: a stream of handles, operations run where the frames live.`,
    },
    components: allComponents,
    nodes: [
      { id: 'camera', component: tensorRef('camera-source'), label: 'Camera' },
      { id: 'edges', component: tensorRef('tensor-stream-edge'), label: `Edges (${where})` },
      { id: 'invert', component: tensorRef('tensor-stream-invert'), label: 'Invert' },
      {
        id: 'sink',
        component: tensorRef('tensor-stream-sink'),
        label: 'Sink',
        attrs: { 'inject.read-last': 'true' },
      },
    ],
    connections: [
      {
        id: 'camera-edges',
        from: { node: 'camera', port: 'tensors' },
        to: { node: 'edges', port: 'tensors' },
      },
      {
        id: 'edges-invert',
        from: { node: 'edges', port: 'tensors' },
        to: { node: 'invert', port: 'tensors' },
      },
      {
        id: 'invert-sink',
        from: { node: 'invert', port: 'tensors' },
        to: { node: 'sink', port: 'tensors' },
      },
    ],
  };
}

/** A graph the demo can load, and the node to select. What it runs on: `session-worker.ts`. */
interface Demo {
  graph: GraphJson;
  select: string;
}

/** A node to make spin: it cannot freeze the page, as it is not on the page's thread. */
const spin: GraphJson = {
  metadata: { name: 'demo-spin', description: 'A node that misbehaves on request.' },
  components: allComponents,
  nodes: [
    {
      id: 'busy',
      component: {
        package: { namespace: 'test', name: 'busy-loop', version: '0.1.0' },
        world: 'busy-loop',
      },
      label: 'Busy loop',
    },
  ],
  connections: [],
};

/** Two nodes of a guest built with debug info: stop on a line of it, and step through. */
const lines: GraphJson = {
  metadata: { name: 'demo-lines', description: 'A guest with debug info, to step through.' },
  components: allComponents,
  nodes: [
    {
      id: 'first',
      component: lineWalkRef,
      label: 'First',
      attrs: { 'inject.in': '5' },
    },
    { id: 'second', component: lineWalkRef, label: 'Second' },
  ],
  connections: [
    {
      id: 'first-second',
      from: { node: 'first', port: 'out' },
      to: { node: 'second', port: 'in' },
    },
  ],
};

const demos: Record<string, Demo> = {
  spin: { graph: spin, select: 'busy' },
  lines: { graph: lines, select: 'first' },
  'port-kinds': {
    graph: portKinds,
    select: 'relay-a',
  },
  video: {
    graph: video,
    select: 'gray',
  },
  webgpu: { graph: edgeGraph('WebGPU'), select: 'edges' },
  webnn: { graph: edgeGraph('WebNN'), select: 'edges' },
  tensors: { graph: tensorGraph('CPU'), select: 'edges' },
  'tensors-webgpu': { graph: tensorGraph('WebGPU'), select: 'edges' },
  'tensors-webnn': { graph: tensorGraph('WebNN'), select: 'edges' },
  camera: { graph: cameraGraph('CPU'), select: 'edges' },
  'camera-webgpu': { graph: cameraGraph('WebGPU'), select: 'edges' },
  'camera-webnn': { graph: cameraGraph('WebNN'), select: 'edges' },
};

// -- Load and display ---------------------------------------------------------

/**
 * A guest's source, by the path its debug info gives. The dev server can read it, if it is under
 * the repository. A built page has no such server, and shows only which lines have code.
 */
async function sources(file: string): Promise<string | undefined> {
  const response = await fetch(`/@fs${file}`);
  const type = response.headers.get('content-type') ?? '';
  return response.ok && !type.includes('html') ? response.text() : undefined;
}

async function fetchWasm(name: string): Promise<Uint8Array> {
  const url = new URL(`../components/${name}.wasm`, import.meta.url);
  const resp = await fetch(url);
  if (!resp.ok)
    throw new Error(`Could not fetch ${name}.wasm (${resp.status}). Run \`pnpm setup\`.`);
  return new Uint8Array(await resp.arrayBuffer());
}

/** The demo the page's `?graph=` names, the port kinds one by default. */
function chosenDemo(): [string, Demo] {
  const name = new URLSearchParams(location.search).get('graph') ?? 'port-kinds';
  return name in demos ? [name, demos[name]] : ['port-kinds', demos['port-kinds']];
}

/** The graph picker: choosing another loads the page with it. */
function setUpPicker(current: string): void {
  const picker = document.getElementById('graph-picker') as HTMLSelectElement | null;
  if (!picker) return;
  picker.value = current;
  picker.addEventListener('change', () => {
    const url = new URL(location.href);
    url.searchParams.set('graph', picker.value);
    location.assign(url);
  });
}

async function loadDemo(): Promise<void> {
  const [name, demo] = chosenDemo();
  setUpPicker(name);
  statusEl.textContent = 'Fetching test components...';

  try {
    const components: ComponentSource[] = await Promise.all(
      GUESTS.map(async (guest) => ({
        id: `test:${guest}/${guest}@0.1.0`,
        bytes: await fetchWasm(guest),
      })),
    );

    statusEl.textContent = 'Loading graph (initialising runtime, transpiling components)...';

    const wasmUrl = new URL('../../witgraph-loader/pkg/witgraph_web_bg.wasm', import.meta.url);
    const wasmResp = await fetch(wasmUrl);
    if (!wasmResp.ok) {
      throw new Error(
        `Could not fetch witgraph_web_bg.wasm (${wasmResp.status}). ` +
          'Run `cd packages/witgraph-loader && pnpm build:wasm` first.',
      );
    }
    const wasm = new Uint8Array(await wasmResp.arrayBuffer());

    const worker = () => {
      // Named for its demo, which says what its capabilities run on.
      const made = new Worker(new URL('./session-worker.ts', import.meta.url), {
        type: 'module',
        name,
      });
      serveCameraFrames(made);
      return made;
    };

    await controller.load({ graph: demo.graph, components, wasm, worker, lines: true, sources });

    statusEl.textContent = 'Computing layout...';
    await controller.autoLayout();

    controller.select(demo.select);

    controller.addEventListener('state-changed', updateStatus);
    updateStatus();
  } catch (err: unknown) {
    showError(err);
  }
}

function updateStatus(): void {
  const error = controller.compileError;
  const mode = controller.mode;
  // A step can leave a stream going, with no run under way.
  const running = [...controller.nodes.values()].filter((node) => node.phase === 'running');
  let state: 'ready' | 'running' | 'paused' | 'error';
  let text: string;
  if (error) {
    state = 'error';
    text = `Graph error: ${error}`;
  } else if (mode === 'running') {
    state = 'running';
    text = `Running — tick ${controller.tickCount}. Click Stop to end the run.`;
  } else if (mode === 'paused') {
    state = 'paused';
    const node = controller.pausedNode;
    const line = controller.lineStop;
    text = line
      ? `Stopped at line ${line.line}${node ? ` in ${node}` : ''}. Step goes to the next line, Continue to the next breakpoint.`
      : `Paused${node ? ` at ${node}` : ''}. Step runs it, Continue goes on to the next breakpoint.`;
  } else if (mode === 'error') {
    state = 'error';
    text = 'The run stopped on an error. The timeline says what failed.';
  } else if (running.length > 0) {
    state = 'running';
    text = `Still running — ${running.length} of ${controller.nodes.size} nodes. Click Stop to end them.`;
  } else {
    state = 'ready';
    text = `Graph compiles — ${controller.nodes.size} nodes, ${controller.edges.length} edges. Click Run or Step to execute.`;
  }
  // A run changes this every tick: touch the page only when it differs.
  if (statusEl.textContent !== text) statusEl.textContent = text;
  for (const name of ['ready', 'running', 'paused', 'error'])
    statusEl.classList.toggle(name, name === state);
}

function showError(err: unknown): void {
  const msg = err instanceof Error ? err.message : String(err);
  statusEl.innerHTML = '';
  const title = document.createElement('strong');
  title.textContent = 'Failed to load demo: ';
  statusEl.appendChild(title);
  statusEl.appendChild(document.createTextNode(msg));
  const instructions = document.createElement('pre');
  instructions.textContent = [
    '',
    'Setup steps (from the repository root):',
    '  1. cargo build -p test-components',
    '  2. cd packages/witgraph-loader && pnpm build:wasm',
    '  3. cd packages/witgraph-editor-demo && pnpm setup',
    '  4. pnpm dev',
  ].join('\n');
  statusEl.appendChild(instructions);
  statusEl.classList.add('error');
  console.error('Demo load failed:', err);
}

loadDemo();
