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
  type WitgraphCanvas,
  type WitgraphCatalog,
  type WitgraphControls,
  type WitgraphInspector,
  type WitgraphTimeline,
} from 'witgraph-editor';
import type { Capability, ComponentSource } from 'witgraph-loader';

registerAll();

const controller = new EditorController();

const canvas = document.querySelector('witgraph-canvas') as WitgraphCanvas;
const controls = document.querySelector('witgraph-controls') as WitgraphControls;
const inspector = document.querySelector('witgraph-inspector') as WitgraphInspector;
const timeline = document.querySelector('witgraph-timeline') as WitgraphTimeline;
const catalog = document.querySelector('witgraph-catalog') as WitgraphCatalog;
const statusEl = document.getElementById('status')!;

canvas.controller = controller;
controls.controller = controller;
inspector.controller = controller;
timeline.controller = controller;
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
];

const allComponents = GUESTS.map((guest) => ({
  package: { namespace: 'test', name: guest, version: '0.1.0' },
  world: guest,
}));

const portKinds: GraphJson = {
  metadata: {
    name: 'demo-all-port-kinds',
    description:
      'Showcasing Value, Stream, Future, external capabilities, mock support and in-node previews with real WASM components.',
  },
  components: allComponents,
  nodes: [
    // Value chain island
    {
      id: 'relay-a',
      component: relayRef,
      label: 'Relay A',
      attrs: { 'inject.in': '0', 'inject.add': '1' },
    },
    { id: 'relay-b', component: relayRef, label: 'Relay B', attrs: { 'inject.add': '0' } },
    { id: 'relay-c', component: relayRef, label: 'Relay C', attrs: { 'inject.add': '0' } },
    // Stream island
    {
      id: 'producer',
      component: streamProducerRef,
      label: 'Producer',
      attrs: { 'inject.burst-size': '5' },
    },
    { id: 'consumer', component: streamConsumerRef, label: 'Consumer' },
    // Future island
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
    // Video island: frames streamed through a filter, each node previewing them
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

/** A graph the demo can load, and the node to select. */
interface Demo {
  graph: GraphJson;
  select: string;
}

const demos: Record<string, Demo> = {
  'port-kinds': {
    graph: portKinds,
    select: 'relay-a',
  },
  video: {
    graph: video,
    select: 'gray',
  },
};

// -- External capabilities (JS implementations for host-provided imports) -----

/**
 * Implements `test:math/ops@0.1.0` in pure JS for the `calc` component.
 * Demonstrates sync, async, and streaming capability provision.
 */
function mathOps() {
  let callCount = 0;
  return {
    double: (x: number) => {
      callCount++;
      return x * 2;
    },
    slowDouble: async (x: number) => x * 2,
    count: async function* (n: number) {
      for (let i = 0; i < n; i++) yield i;
    },
    calls: () => callCount,
  };
}

/**
 * Implements `test:mqtt/source@0.1.0` in pure JS for the `mqtt-node`.
 * Returns canned sensor readings as byte buffers, then `undefined` to
 * signal end-of-feed.
 */
function mqttSource() {
  const feed = [
    new Uint8Array([0x48, 0x45, 0x4c, 0x4c, 0x4f]), // "HELLO"
    new Uint8Array([0x74, 0x65, 0x6d, 0x70, 0x3a, 0x32, 0x33]), // "temp:23"
    new Uint8Array([0x6f, 0x6b]), // "ok"
  ];
  let index = 0;
  return {
    nextMessage: () => (index < feed.length ? feed[index++] : undefined),
  };
}

/** Implements `test:video/clock@0.1.0` for the `video-source` component. */
const clock = {
  sleep: (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)),
};

/** Per-node capability instances (fresh state per instantiation). */
const capInstances = new Map<string, Record<string, object>>();

function capabilities(node: string, capability: Capability): object | undefined {
  const iface = capability.implements ?? capability.interface;
  if (iface.startsWith('test:video/clock')) return clock;
  if (iface.startsWith('test:math/ops')) {
    let instances = capInstances.get(node);
    if (!instances) {
      instances = {};
      capInstances.set(node, instances);
    }
    if (!instances[iface]) instances[iface] = mathOps();
    return instances[iface];
  }
  if (iface.startsWith('test:mqtt/source')) {
    let instances = capInstances.get(node);
    if (!instances) {
      instances = {};
      capInstances.set(node, instances);
    }
    if (!instances[iface]) instances[iface] = mqttSource();
    return instances[iface];
  }
  return undefined;
}

// -- Load and display ---------------------------------------------------------

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

    await controller.load({ graph: demo.graph, components, wasm, capabilities });

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
  statusEl.classList.remove('ready', 'error');
  if (error) {
    statusEl.textContent = `Graph error: ${error}`;
    statusEl.classList.add('error');
  } else {
    statusEl.textContent = `Graph compiles -- ${controller.nodes.size} nodes, ${controller.edges.length} edges. Click Run or Step to execute.`;
    statusEl.classList.add('ready');
  }
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
