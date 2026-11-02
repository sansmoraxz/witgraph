// Integration tests for EditorController using the actual witgraph-loader
// with test component fixtures. Build the guests first:
//   cargo build -p test-components
//   cd packages/witgraph-loader && npm run build:wasm

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { afterEach, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import type { Capability, ComponentSource } from 'witgraph-loader';
import { EditorController } from '../src/controller/editor-controller.ts';
import type { EditorLoadOptions, GraphJson } from '../src/controller/types.ts';
import { registerViewer } from '../src/viewers/registry.ts';

// ---------------------------------------------------------------------------
// Helpers (same pattern as witgraph-loader/test/session.test.ts)
// ---------------------------------------------------------------------------

const root = new URL('../../../', import.meta.url);

function targetDir(): URL {
  const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: fileURLToPath(root),
    encoding: 'utf8',
  });
  return pathToFileURL(`${JSON.parse(metadata).target_directory}/`);
}

async function guestDir(): Promise<URL> {
  const build = new URL('debug/build/', targetDir());
  const candidates: [number, URL][] = [];
  for (const name of await readdir(build).catch(() => [])) {
    if (!name.startsWith('test-components-')) continue;
    const out = new URL(`${name}/out/`, build);
    const echo = await stat(new URL('echo.wasm', out)).catch(() => undefined);
    if (echo) candidates.push([echo.mtimeMs, out]);
  }
  candidates.sort((a, b) => b[0] - a[0]);
  assert.ok(candidates.length > 0, 'build the guests with `cargo build -p test-components`');
  return candidates[0][1];
}

const guests = await guestDir();
const wasm = await readFile(
  new URL('../../witgraph-loader/pkg/witgraph_web_bg.wasm', import.meta.url),
);

const ids: Record<string, string> = {
  echo: 'test:echo/echo@0.1.0',
  relay: 'test:relay/relay@0.1.0',
  'video-source': 'test:video-source/video-source@0.1.0',
  'frame-grayscale': 'test:frame-grayscale/frame-grayscale@0.1.0',
  'video-sink': 'test:video-sink/video-sink@0.1.0',
  'stream-producer': 'test:stream-producer/stream-producer@0.1.0',
  'stream-consumer': 'test:stream-consumer/stream-consumer@0.1.0',
  'future-writer': 'test:future-writer/future-writer@0.1.0',
  'future-reader': 'test:future-reader/future-reader@0.1.0',
  'tensor-source': 'test:tensor-source/tensor-source@0.1.0',
  'tensor-stream-source': 'test:tensor-stream-source/tensor-stream-source@0.1.0',
  'tensor-stream-invert': 'test:tensor-stream-invert/tensor-stream-invert@0.1.0',
  'tensor-stream-sink': 'test:tensor-stream-sink/tensor-stream-sink@0.1.0',
  'tensor-future-source': 'test:tensor-future-source/tensor-future-source@0.1.0',
  'tensor-future-sink': 'test:tensor-future-sink/tensor-future-sink@0.1.0',
};

function componentRef(id: string) {
  const match = /^([^:]+):([^/]+)\/([^@]+)@(.+)$/.exec(id)!;
  return { package: { namespace: match[1], name: match[2], version: match[4] }, world: match[3] };
}

async function guestSource(guest: string): Promise<ComponentSource> {
  return {
    id: ids[guest],
    bytes: new Uint8Array(await readFile(new URL(`${guest}.wasm`, guests))),
  };
}

/** Every controller a test made, disposed after it. */
const controllers: EditorController[] = [];
afterEach(() => {
  for (const c of controllers.splice(0)) c.dispose();
});

type Connection = [id: string, from: string, to: string, feedback?: boolean];

/** Loads an EditorController over the named guests, disposed after the test. */
async function loadController(
  nodes: [string, string][],
  connections: Connection[] = [],
  options: Partial<EditorLoadOptions> = {},
): Promise<EditorController> {
  const used = [...new Set(nodes.map(([, guest]) => guest))];
  const components = await Promise.all(used.map(guestSource));
  const port = (text: string) => {
    const [node, name] = text.split('.');
    return { node, port: name };
  };
  const graph: GraphJson = {
    metadata: { name: 'test' },
    components: components.map(({ id }) => componentRef(id)),
    nodes: nodes.map(([id, guest]) => ({ id, component: componentRef(ids[guest]) })),
    connections: connections.map(([id, from, to, feedback]) => ({
      id,
      from: port(from),
      to: port(to),
      ...(feedback ? { feedback: true } : {}),
    })),
  };
  const controller = new EditorController();
  await controller.load({ graph, components, wasm, ...options });
  controllers.push(controller);
  return controller;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test('load and dispose', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [['c', 'a.out', 'b.in']],
  );
  assert.equal(controller.nodes.size, 2);
  assert.ok(controller.nodes.has('a'));
  assert.ok(controller.nodes.has('b'));
  assert.equal(controller.edges.length, 1);
  assert.equal(controller.mode, 'idle');

  controller.dispose();
  controllers.splice(controllers.indexOf(controller), 1);
  assert.equal(controller.nodes.size, 0);
});

test('step and read output', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.injectValue('a', 'in', '5');
  await controller.step();
  assert.equal(controller.readOutputWave('a', 'out'), '5');
});

test('run settles the graph', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [['c', 'a.out', 'b.in']],
  );
  controller.injectValue('a', 'in', '5');
  await controller.run();
  assert.equal(controller.readOutputWave('a', 'out'), '5');
  assert.equal(controller.readOutputWave('b', 'out'), '5');
});

test('breakpoint pauses execution', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.toggleBreakpoint('a');
  controller.injectValue('a', 'in', '5');

  const paused = new Promise<void>((resolve) => {
    controller.addEventListener('state-changed', function handler() {
      if (controller.mode === 'paused') {
        controller.removeEventListener('state-changed', handler);
        resolve();
      }
    });
  });

  const runPromise = controller.run();
  await paused;
  assert.equal(controller.mode, 'paused');

  controller.resumeBreakpoint();
  await runPromise;
  assert.equal(controller.mode, 'idle');
});

test('moveNode updates position', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.moveNode('a', 100, 200);

  const node = controller.nodes.get('a')!;
  assert.equal(node.x, 100);
  assert.equal(node.y, 200);

  const graphJson = controller.graphJson as GraphJson;
  const graphNode = graphJson.nodes.find((n) => n.id === 'a')!;
  assert.equal(graphNode.attrs!['editor.x'], '100');
  assert.equal(graphNode.attrs!['editor.y'], '200');
});

test('select dispatches event', async () => {
  const controller = await loadController([['a', 'echo']]);

  let changed = false;
  controller.addEventListener('state-changed', () => {
    changed = true;
  });

  controller.select('a');
  assert.equal(controller.selectedNode, 'a');
  // state-changed is dispatched via queueMicrotask — await it
  await new Promise<void>((r) => queueMicrotask(r));
  assert.ok(changed, 'state-changed event should have fired');
});

test('autoLayout positions nodes', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [['c', 'a.out', 'b.in']],
  );
  await controller.autoLayout();

  const posA = controller.nodes.get('a')!;
  const posB = controller.nodes.get('b')!;
  assert.ok(posA.x >= 0, 'a.x non-negative');
  assert.ok(posA.y >= 0, 'a.y non-negative');
  assert.ok(posB.x >= 0, 'b.x non-negative');
  assert.ok(posB.y >= 0, 'b.y non-negative');
  assert.ok(posA.x !== posB.x || posA.y !== posB.y, 'nodes should not share the same position');
});

test('trace events are collected', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.injectValue('a', 'in', '5');
  await controller.run();
  assert.ok(controller.traceEvents.length > 0, 'should have trace events after a run');
});

// ---------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------

/** `test:video/clock`, sleeping on the event loop. */
const clock = (_node: string, capability: Capability) =>
  capability.interface === 'test:video/clock@0.1.0'
    ? { sleep: (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)) }
    : undefined;

interface Frame {
  width: number;
  height: number;
  pixels: Uint8Array;
}

/** source → gray → sink. */
function loadVideo(): Promise<EditorController> {
  return loadController(
    [
      ['source', 'video-source'],
      ['gray', 'frame-grayscale'],
      ['sink', 'video-sink'],
    ],
    [
      ['a', 'source.frames', 'gray.frames'],
      ['b', 'gray.frames', 'sink.frames'],
    ],
    { capabilities: clock },
  );
}

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

test('a node previews the output its component declares a viewer for', async () => {
  const controller = await loadVideo();
  for (const node of ['source', 'gray']) {
    const preview = controller.nodes.get(node)?.preview;
    assert.deepEqual([preview?.port, preview?.viewer], ['frames', 'image'], node);
  }
  assert.equal(controller.nodes.get('sink')?.preview, undefined);

  let changes = 0;
  controller.addEventListener('output-changed', () => changes++);
  controller.injectValue('source', 'frames', '3');
  await controller.run();
  await pause(50);
  assert.ok(changes >= 1, 'stream items are announced');
  assert.equal(controller.latestOutput('sink', 'count'), 3);

  // Row 40, column 50: below the moving square.
  const rgb = (node: string) => {
    const frame = controller.latestOutput(node, 'frames') as Frame;
    assert.deepEqual([frame.width, frame.height], [96, 64], node);
    const at = (40 * 96 + 50) * 4;
    return [...frame.pixels.subarray(at, at + 3)];
  };
  const [r, g, b] = rgb('gray');
  assert.ok(r === g && g === b, `gray: ${[r, g, b]}`);
  const [cr, cg, cb] = rgb('source');
  assert.ok(cr !== cg || cg !== cb, `in colour: ${[cr, cg, cb]}`);
});

test('a node is resized and its preview picked by undoable edits', async () => {
  const controller = await loadVideo();
  const before = structuredClone(controller.nodes.get('gray')!);
  controller.resizeNode('gray', 300, 420);
  const resized = controller.nodes.get('gray')!;
  assert.deepEqual([resized.width, resized.height], [300, 420]);
  assert.equal(resized.preview?.height, 420 - 32 - resized.portsHeight);
  controller.undo();
  const undone = controller.nodes.get('gray')!;
  assert.deepEqual([undone.width, undone.height], [before.width, before.height]);

  controller.setPreview('gray', 'none');
  assert.equal(controller.nodes.get('gray')!.preview, undefined);
  assert.ok(controller.nodes.get('gray')!.height < before.height);
  controller.setPreview('gray', null);
  assert.equal(controller.nodes.get('gray')!.preview?.port, 'frames');

  // A node with no preview keeps the height its ports give it.
  const sink = controller.nodes.get('sink')!;
  controller.resizeNode('sink', 280, 500);
  assert.deepEqual(
    [controller.nodes.get('sink')!.width, controller.nodes.get('sink')!.height],
    [280, sink.height],
  );
  const sinkAttrs = (controller.graphJson as GraphJson).nodes.find((n) => n.id === 'sink')!.attrs!;
  assert.equal(sinkAttrs['editor.h'], undefined);

  // The size and preview are the graph's: they survive a reload.
  controller.resizeNode('gray', 260, 300);
  controller.setPreview('source', 'none');
  await controller.importGraph(controller.exportGraph());
  assert.equal(controller.nodes.get('gray')!.width, 260);
  assert.equal(controller.nodes.get('source')!.preview, undefined);
});

test('stop ends a stream still being written, and run starts it again', async () => {
  const controller = await loadVideo();
  // Endless, paced: a run lasts until it is stopped.
  controller.injectValue('source', 'fps', '200');
  const frame = () => controller.latestOutput('gray', 'frames');
  const waitForFrame = async (previous: unknown) => {
    for (let i = 0; i < 400 && frame() === previous; i++) await pause(5);
    assert.notEqual(frame(), previous, 'a new frame arrives');
  };

  const running = controller.run();
  await waitForFrame(undefined);
  controller.stop();
  await running;
  const stopped = frame();
  await pause(100);
  assert.equal(frame(), stopped, 'no frame arrives once stopped');

  const again = controller.run();
  await waitForFrame(stopped);
  controller.stop();
  await again;
});

test('a stream output keeps its recent items and their count', async () => {
  const controller = await loadController(
    [
      ['prod', 'stream-producer'],
      ['cons', 'stream-consumer'],
    ],
    [['s', 'prod.items', 'cons.items']],
  );
  assert.equal(controller.nodes.get('prod')?.preview?.viewer, 'series');
  controller.injectValue('prod', 'burst-size', '5');
  await controller.step();
  const items = controller.observe('prod', 'items');
  assert.deepEqual([items.kind, items.count, items.latest], ['stream', 5, 4]);
  assert.deepEqual(items.recent, [0, 1, 2, 3, 4]);

  controller.injectValue('prod', 'burst-size', '3');
  await controller.step();
  const more = controller.observe('prod', 'items');
  assert.equal(more.count, 8, 'counted across runs');
  assert.deepEqual(more.recent, [0, 1, 2, 3, 4, 0, 1, 2]);

  // Only the newest are kept; all are counted.
  controller.injectValue('prod', 'burst-size', '500');
  await controller.run();
  const many = controller.observe('prod', 'items');
  assert.equal(many.count, 508);
  assert.equal(many.recent.length, 120);
  assert.equal(many.latest, 499);
});

test('a future output is pending until it resolves', async () => {
  const controller = await loadController(
    [
      ['w', 'future-writer'],
      ['r', 'future-reader'],
    ],
    [['c', 'w.done', 'r.data']],
  );
  assert.deepEqual(controller.observe('w', 'done'), {
    kind: 'future',
    latest: undefined,
    recent: [],
    count: 0,
    pending: false,
  });
  await controller.step();
  for (let i = 0; i < 50 && controller.observe('w', 'done').count === 0; i++) await pause(10);
  const done = controller.observe('w', 'done');
  assert.deepEqual([done.latest, done.count, done.pending], [7, 1, false]);
  assert.equal(controller.latestOutput('r', 'value'), 7);
});

test('a Value output keeps one value a tick', async () => {
  const controller = await loadController([['n', 'relay']], [['loop', 'n.out', 'n.in', true]]);
  assert.equal(controller.nodes.get('n')?.preview?.viewer, 'series');
  controller.injectValue('n', 'in', '1');
  controller.injectValue('n', 'add', '2');
  for (let i = 0; i < 3; i++) await controller.step();
  const out = controller.observe('n', 'out');
  assert.equal(out.kind, 'value');
  assert.deepEqual(out.recent, [3, 5, 7]);
  assert.equal(out.latest, 7);

  // Observing again shows the same: the value recorded is not shown twice.
  for (let i = 0; i < 3; i++) {
    const again = controller.observe('n', 'out');
    assert.deepEqual([again.recent, again.latest], [[3, 5, 7], 7]);
  }
});

test('a run that never settles goes on until it is stopped', async () => {
  const controller = await loadController([['n', 'relay']], [['loop', 'n.out', 'n.in', true]]);
  controller.injectValue('n', 'in', '0');
  controller.injectValue('n', 'add', '1');
  const running = controller.run();
  await pause(600);
  assert.equal(controller.mode, 'running');
  const before = controller.observe('n', 'out').count;
  assert.ok(before > 5, `the loop advances: ${before} ticks`);
  controller.stop();
  await running;
  assert.equal(controller.mode, 'idle');
});

test('a step is one bounded tick; what it left running stops on stop', async () => {
  const controller = await loadVideo();
  controller.injectValue('source', 'fps', '100');
  const started = performance.now();
  await controller.step();
  assert.ok(performance.now() - started < 2000, 'the endless stream does not hold the step');
  assert.equal(controller.mode, 'idle');
  // The stream carries on after the step...
  const first = controller.observe('gray', 'frames').count;
  await pause(200);
  const later = controller.observe('gray', 'frames').count;
  assert.ok(later > first, `frames keep coming: ${first} then ${later}`);
  // ...until it is stopped.
  controller.stop();
  await pause(50);
  const stopped = controller.observe('gray', 'frames').count;
  await pause(200);
  assert.equal(controller.observe('gray', 'frames').count, stopped);
});

test('a run ticks no faster than its tick rate, which can change as it runs', async () => {
  const controller = await loadController([['n', 'relay']], [['loop', 'n.out', 'n.in', true]]);
  controller.injectValue('n', 'in', '0');
  controller.injectValue('n', 'add', '1');
  controller.tickRate = 5;
  const running = controller.run();
  await pause(1_000);
  const slow = controller.observe('n', 'out').count;
  assert.ok(slow >= 3 && slow <= 7, `about 5 ticks in a second: ${slow}`);

  controller.tickRate = 0;
  await pause(500);
  const fast = controller.observe('n', 'out').count - slow;
  assert.ok(fast > 10, `unlimited, many more in half a second: ${fast}`);
  controller.stop();
  await running;

  controller.tickRate = -3;
  assert.equal(controller.tickRate, 0, 'anything but a positive rate is no limit');
});

test('only what is previewed or selected is recorded', async () => {
  const controller = await loadController([['n', 'relay']], [['loop', 'n.out', 'n.in', true]]);
  controller.injectValue('n', 'in', '0');
  controller.injectValue('n', 'add', '1');
  controller.setPreview('n', 'none');
  controller.select(null);
  assert.equal(controller.isObserved('n', 'out'), false);
  await controller.step();
  await controller.step();
  assert.equal(controller.observe('n', 'out').count, 0, 'nothing recorded');
  // Its latest value is still read when asked for.
  assert.equal(controller.latestOutput('n', 'out'), 2);

  // Selected, the inspector shows it: it is recorded from then on.
  controller.select('n');
  assert.equal(controller.isObserved('n', 'out'), true);
  await controller.step();
  assert.equal(controller.observe('n', 'out').count, 1);
});

/** `test:data/tensor` in host memory, counting what is left undisposed. */
function cpuTensors() {
  const live = new Set<object>();
  class Tensor {
    readonly label: string;
    constructor(label = 'made') {
      this.label = label;
      live.add(this);
    }
    static async fromBytes() {
      return new Tensor();
    }
    dtype() {
      return 'uint8';
    }
    shape() {
      return [1];
    }
    async read() {
      return new Uint8Array(1);
    }
    clone() {
      return new Tensor('kept');
    }
    async invert() {
      return new Tensor();
    }
    [Symbol.dispose]() {
      live.delete(this);
    }
  }
  return {
    live,
    capabilities: (_: string, capability: Capability) =>
      capability.interface === 'test:data/tensor@0.1.0' ? { Tensor } : undefined,
  };
}

const streamOfTensors: [[string, string][], Connection[]] = [
  [
    ['src', 'tensor-stream-source'],
    ['inv', 'tensor-stream-invert'],
    ['sink', 'tensor-stream-sink'],
  ],
  [
    ['a', 'src.tensors', 'inv.tensors'],
    ['b', 'inv.tensors', 'sink.tensors'],
  ],
];

const settled = async (controller: EditorController) => {
  for (let i = 0; i < 20; i++) await controller.step();
};

test('a stream of handles connects only to a stream of that resource', async () => {
  const tensors = cpuTensors();
  const controller = await loadController(
    [
      ['src', 'tensor-stream-source'],
      ['inv', 'tensor-stream-invert'],
      ['one', 'tensor-source'],
      ['fut', 'tensor-future-source'],
      ['nums', 'stream-producer'],
      ['sink', 'tensor-stream-sink'],
      ['take', 'stream-consumer'],
    ],
    [
      ['a', 'src.tensors', 'inv.tensors'],
      ['b', 'inv.tensors', 'sink.tensors'],
      ['c', 'nums.items', 'take.items'],
    ],
    { capabilities: tensors.capabilities },
  );
  const port = controller.nodes.get('src')?.outputs.find((p) => p.name === 'tensors');
  assert.equal(port?.kind, 'stream');
  assert.equal(port?.typeDisplay, 'tensor');
  assert.equal(controller.canConnect('src', 'tensors', 'sink', 'tensors').ok, true);
  assert.match(
    controller.canConnect('one', 'tensor', 'sink', 'tensors').reason ?? '',
    /kind mismatch/,
    'a handle port is not a stream',
  );
  assert.match(
    controller.canConnect('fut', 'tensor', 'sink', 'tensors').reason ?? '',
    /kind mismatch/,
    'nor is a future',
  );
  assert.match(
    controller.canConnect('nums', 'items', 'sink', 'tensors').reason ?? '',
    /resource mismatch/,
    'a stream of numbers is not a stream of tensors',
  );
  assert.equal(controller.canConnect('src', 'tensors', 'take', 'items').ok, false);
  assert.deepEqual(
    controller.compatibleOutputsForInput('sink', 'tensors').map((o) => o.node),
    ['src', 'inv'],
  );
  assert.equal(controller.canBeFeedback('src', 'tensors'), false);
});

test('a preview keeps a reference to a stream of handles, one at a time', async () => {
  const tensors = cpuTensors();
  const kept: unknown[] = [];
  registerViewer({
    id: 'tensor',
    mount: () => ({ update() {}, dispose() {} }),
    keep(item) {
      const reference = (item as { clone(): unknown }).clone();
      kept.push(reference);
      return reference;
    },
  });
  const controller = await loadController(...streamOfTensors, {
    capabilities: tensors.capabilities,
  });
  controller.injectValue('src', 'frames', '4');
  await settled(controller);
  const view = controller.observe('src', 'tensors');
  assert.equal(view.count, 4);
  assert.equal(kept.length >= 4, true, 'each node previewing its stream keeps its own');
  assert.ok(kept.includes(view.latest));
  assert.equal((view.latest as { label: string }).label, 'kept');
  assert.deepEqual(view.recent, [view.latest], 'no history holding on to them');
  // Only the previews' latest references are left, not what the nodes passed.
  assert.equal(tensors.live.size, 2, 'the latest of `src` and of `inv`');
  assert.ok(tensors.live.has(view.latest as object));

  controller.dispose();
  controllers.splice(controllers.indexOf(controller), 1);
  assert.equal(tensors.live.size, 0, 'what was kept goes with the graph');
});

test('without a way to keep them, a preview describes a stream of handles', async () => {
  const tensors = cpuTensors();
  registerViewer({ id: 'tensor', mount: () => ({ update() {}, dispose() {} }) });
  const controller = await loadController(...streamOfTensors, {
    capabilities: tensors.capabilities,
  });
  controller.injectValue('src', 'frames', '2');
  await settled(controller);
  const view = controller.observe('src', 'tensors');
  assert.equal(view.count, 2);
  assert.equal(view.latest, '<tensor>');
  assert.equal(tensors.live.size, 0, 'it holds on to none');
});

test('a future of a handle is kept and released like a stream item', async () => {
  const tensors = cpuTensors();
  registerViewer({
    id: 'tensor',
    mount: () => ({ update() {}, dispose() {} }),
    keep: (item) => (item as { clone(): unknown }).clone(),
  });
  const controller = await loadController(
    [
      ['src', 'tensor-future-source'],
      ['sink', 'tensor-future-sink'],
    ],
    [['a', 'src.tensor', 'sink.tensor']],
    { capabilities: tensors.capabilities },
  );
  controller.setPreview('src', 'tensor');
  controller.select('src');
  await settled(controller);
  const view = controller.observe('src', 'tensor');
  assert.equal(view.count, 1);
  assert.equal(view.pending, false);
  assert.equal(typeof view.latest, 'string', 'its component names no viewer: described');
  assert.equal(tensors.live.size, 0);
});
