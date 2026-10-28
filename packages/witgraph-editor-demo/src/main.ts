/**
 * WIT Graph Editor — Demo
 *
 * Three islands showcasing all port kinds (Value, Stream, Future),
 * running real WASM components via jco — no mocks.
 *
 * Prerequisites:
 *   cargo build -p test-components
 *   cd packages/witgraph-loader && pnpm build:wasm
 *   cd packages/witgraph-editor-demo && pnpm setup && pnpm dev
 */

import {
  EditorController,
  registerAll,
  type WitgraphCanvas,
  type WitgraphControls,
  type WitgraphInspector,
  type WitgraphTimeline,
  type WitgraphCatalog,
  type GraphJson,
} from 'witgraph-editor';
import type { ComponentSource } from 'witgraph-loader';

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

// -- Graph definition ---------------------------------------------------------

const graph: GraphJson = {
  metadata: {
    name: 'demo-all-port-kinds',
    description: 'Three islands showcasing Value, Stream, and Future port kinds with real WASM components.',
  },
  components: [relayRef, streamProducerRef, streamConsumerRef, futureWriterRef, futureReaderRef],
  nodes: [
    { id: 'relay-a', component: relayRef, label: 'Relay A',
      attrs: { 'inject.in': '0', 'inject.add': '1' } },
    { id: 'relay-b', component: relayRef, label: 'Relay B',
      attrs: { 'inject.add': '0' } },
    { id: 'relay-c', component: relayRef, label: 'Relay C',
      attrs: { 'inject.add': '0' } },
    { id: 'producer', component: streamProducerRef, label: 'Producer',
      attrs: { 'inject.burst-size': '5' } },
    { id: 'consumer', component: streamConsumerRef, label: 'Consumer' },
    { id: 'writer', component: futureWriterRef, label: 'Future Writer' },
    { id: 'reader', component: futureReaderRef, label: 'Future Reader' },
    { id: 'display', component: relayRef, label: 'Display',
      attrs: { 'inject.add': '0' } },
  ],
  connections: [
    { id: 'a-feedback', from: { node: 'relay-a', port: 'out' }, to: { node: 'relay-a', port: 'in' }, feedback: true },
    { id: 'a-to-b', from: { node: 'relay-a', port: 'out' }, to: { node: 'relay-b', port: 'in' } },
    { id: 'b-to-c', from: { node: 'relay-b', port: 'out' }, to: { node: 'relay-c', port: 'in' } },
    { id: 'stream', from: { node: 'producer', port: 'items' }, to: { node: 'consumer', port: 'items' } },
    { id: 'future-link', from: { node: 'writer', port: 'done' }, to: { node: 'reader', port: 'data' } },
    { id: 'writer-to-display', from: { node: 'writer', port: 'last' }, to: { node: 'display', port: 'in' } },
  ],
};

// -- Load and display ---------------------------------------------------------

async function fetchWasm(name: string): Promise<Uint8Array> {
  const url = new URL(`../components/${name}.wasm`, import.meta.url);
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`Could not fetch ${name}.wasm (${resp.status}). Run \`pnpm setup\`.`);
  return new Uint8Array(await resp.arrayBuffer());
}

async function loadDemo(): Promise<void> {
  statusEl.textContent = 'Fetching test components...';

  try {
    const [relayBytes, prodBytes, consBytes, writerBytes, readerBytes] = await Promise.all([
      fetchWasm('relay'),
      fetchWasm('stream-producer'),
      fetchWasm('stream-consumer'),
      fetchWasm('future-writer'),
      fetchWasm('future-reader'),
    ]);

    const components: ComponentSource[] = [
      { id: 'test:relay/relay@0.1.0', bytes: relayBytes },
      { id: 'test:stream-producer/stream-producer@0.1.0', bytes: prodBytes },
      { id: 'test:stream-consumer/stream-consumer@0.1.0', bytes: consBytes },
      { id: 'test:future-writer/future-writer@0.1.0', bytes: writerBytes },
      { id: 'test:future-reader/future-reader@0.1.0', bytes: readerBytes },
    ];

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

    await controller.load({ graph, components, wasm });

    statusEl.textContent = 'Computing layout...';
    await controller.autoLayout();

    controller.select('relay-a');

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
    '', 'Setup steps (from the repository root):',
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
