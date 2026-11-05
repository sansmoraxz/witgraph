// Integration tests for EditorController using the actual witgraph-loader
// with test component fixtures. Build the guests first:
//   cargo build -p test-components
//   cd packages/witgraph-loader && npm run build:wasm

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { after, afterEach, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { MessageChannel, Worker } from 'node:worker_threads';
import { monotonicClock } from '@bytecodealliance/preview3-shim/clocks';
import {
  type Capability,
  type ComponentSource,
  type Endpoint,
  type ServeOptions,
  type SessionWorker,
  serveSession,
} from 'witgraph-loader';
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
  'line-walk': 'test:line-walk/line-walk@0.1.0',
  'busy-loop': 'test:busy-loop/busy-loop@0.1.0',
  echo: 'test:echo/echo@0.1.0',
  relay: 'test:relay/relay@0.1.0',
  'video-source': 'test:video-source/video-source@0.1.0',
  'frame-grayscale': 'test:frame-grayscale/frame-grayscale@0.1.0',
  'video-sink': 'test:video-sink/video-sink@0.1.0',
  'stream-producer': 'test:stream-producer/stream-producer@0.1.0',
  'stream-consumer': 'test:stream-consumer/stream-consumer@0.1.0',
  'stream-relay': 'test:stream-relay/stream-relay@0.1.0',
  'stream-drain': 'test:stream-drain/stream-drain@0.1.0',
  'future-writer': 'test:future-writer/future-writer@0.1.0',
  'future-reader': 'test:future-reader/future-reader@0.1.0',
  'tensor-source': 'test:tensor-source/tensor-source@0.1.0',
  'tensor-invert': 'test:tensor-invert/tensor-invert@0.1.0',
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
const ports: { close(): void }[] = [];
after(() => {
  for (const port of ports.splice(0)) port.close();
});

/**
 * What a session runs over: the worker's side served here, where a test can reach what the
 * worker provides. `packages/witgraph-editor-demo` runs over a real one.
 */
function served(options: ServeOptions = {}): Endpoint {
  const { port1, port2 } = new MessageChannel();
  ports.push(port1, port2);
  serveSession(port2 as unknown as Endpoint, options);
  return port1 as unknown as Endpoint;
}

async function loadController(
  nodes: [string, string][],
  connections: Connection[] = [],
  { capabilities, views, ...options }: Partial<EditorLoadOptions> & ServeOptions = {},
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
  const worker = served({ capabilities, views });
  await controller.load({ graph, components, wasm, worker: () => worker, ...options });
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

test('the catalog describes a component by the docs it carries', async () => {
  // The description is in the component's bytes (a `witgraph-docs` section).
  const components = await Promise.all(['relay', 'echo'].map(guestSource));
  const controller = await loadController([['a', 'relay']], [['c', 'a.out', 'a.in', true]], {
    components,
  });

  const docs = new Map(controller.componentSources.map((source) => [source.id, source.docs]));
  assert.equal(docs.get(ids.relay), 'Passes a u32 through, adding `add` (default 0).');
  // One no node uses is described the same.
  assert.equal(docs.size, 2);
  assert.ok(docs.get(ids.echo), 'echo has docs');
  // And the node shows its component's.
  assert.equal(controller.nodes.get('a')?.docs, docs.get(ids.relay));
});

test('step and read output', async () => {
  const controller = await loadController([['a', 'echo']]);
  await controller.injectValue('a', 'in', '5');
  await controller.tick();
  assert.equal(await controller.readOutputWave('a', 'out'), '5');
});

test('run settles the graph', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [['c', 'a.out', 'b.in']],
  );
  await controller.injectValue('a', 'in', '5');
  await controller.run();
  assert.equal(await controller.readOutputWave('a', 'out'), '5');
  assert.equal(await controller.readOutputWave('b', 'out'), '5');
});

test('breakpoint pauses execution', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.toggleBreakpoint('a');
  await controller.injectValue('a', 'in', '5');

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

test('several nodes at breakpoints at once each wait for their own resume', async () => {
  const controller = await loadController([
    ['a', 'echo'],
    ['b', 'echo'],
  ]);
  controller.toggleBreakpoint('a');
  controller.toggleBreakpoint('b');
  const running = controller.run();
  const held = () => controller.pausedNode;
  for (let at = 0; at < 200 && held() === null; at++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  const first = held();
  assert.ok(first === 'a' || first === 'b', `paused at ${first}`);
  assert.equal(controller.mode, 'paused');
  assert.equal(controller.breakpointState?.node, first);

  // Both reach theirs in one pass, and the second must not take the place of the first.
  for (let at = 0; at < 20; at++) await new Promise((resolve) => setTimeout(resolve, 5));
  controller.resumeBreakpoint();
  assert.equal(controller.mode, 'paused', 'the other is still held');
  assert.equal(held(), first === 'a' ? 'b' : 'a');
  controller.resumeBreakpoint();
  await running;
  assert.equal(controller.mode, 'idle');
  assert.equal(held(), null);
  assert.equal(controller.nodes.get('a')?.phase, 'idle');
  assert.equal(controller.nodes.get('b')?.phase, 'idle');
});

test('stopping releases every node at a breakpoint', async () => {
  const controller = await loadController([
    ['a', 'echo'],
    ['b', 'echo'],
  ]);
  controller.toggleBreakpoint('a');
  controller.toggleBreakpoint('b');
  const running = controller.run();
  for (let at = 0; at < 200 && controller.pausedNode === null; at++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  for (let at = 0; at < 20; at++) await new Promise((resolve) => setTimeout(resolve, 5));
  controller.stop();
  await running;
  assert.equal(controller.pausedNode, null);
  assert.notEqual(controller.mode, 'paused');
});

/** Waits until `ready`, which something else makes so. */
async function eventually(ready: () => boolean): Promise<void> {
  for (let at = 0; at < 400 && !ready(); at++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.ok(ready(), 'it never happened');
}

test('steps walk a graph one node at a time, then start over', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
      ['c', 'echo'],
    ],
    [
      ['ab', 'a.out', 'b.in'],
      ['bc', 'b.out', 'c.in'],
    ],
  );
  const phases = () => [...controller.nodes.values()].map((node) => node.phase);
  await controller.injectValue('a', 'in', '5');

  await controller.step();
  assert.equal(controller.mode, 'paused');
  assert.equal(controller.pausedNode, 'a', 'stopped before anything has run');
  assert.equal(controller.selectedNode, 'a', 'and shown');
  assert.deepEqual(phases(), ['pending', 'pending', 'pending']);
  assert.deepEqual(controller.breakpointState?.inputs, { in: 5 });

  await controller.step();
  assert.equal(controller.pausedNode, 'b');
  assert.deepEqual(phases(), ['idle', 'pending', 'pending']);
  assert.deepEqual(controller.breakpointState?.inputs, { in: 5 });

  await controller.step();
  assert.equal(controller.pausedNode, 'c');
  assert.deepEqual(phases(), ['idle', 'idle', 'pending']);

  await controller.step();
  await eventually(() => controller.mode === 'idle');
  assert.equal(controller.pausedNode, null);
  assert.deepEqual(phases(), ['idle', 'idle', 'idle']);

  // Past the end, the next step starts over, as stepping a finished program would.
  await controller.step();
  assert.equal(controller.mode, 'paused');
  assert.equal(controller.pausedNode, 'a');
  assert.deepEqual(
    controller.traceEvents.filter((e) => e.event === 'error'),
    [],
  );
  controller.stop();
});

test('a step goes on from a breakpoint, and continue goes on to the next', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
      ['c', 'echo'],
    ],
    [
      ['ab', 'a.out', 'b.in'],
      ['bc', 'b.out', 'c.in'],
    ],
  );
  controller.toggleBreakpoint('a');
  controller.toggleBreakpoint('c');
  const running = controller.run();
  await eventually(() => controller.pausedNode === 'a');
  assert.equal(controller.mode, 'paused');

  await controller.step();
  assert.equal(controller.pausedNode, 'b', 'held for the step, with no breakpoint of its own');

  // Continue stops at the next breakpoint.
  await controller.run();
  await eventually(() => controller.pausedNode === 'c');
  assert.equal(controller.nodes.get('b')?.phase, 'idle');

  controller.resumeBreakpoint();
  await running;
  assert.equal(controller.mode, 'idle');
});

test('what finishes during a pause does not go on showing as running', async () => {
  // Started in one pass: when `a` returns, `b` is already waiting to be stepped to.
  const controller = await loadController([
    ['a', 'echo'],
    ['b', 'echo'],
  ]);
  await controller.step();
  assert.equal(controller.pausedNode, 'a');
  await controller.step();
  assert.equal(controller.pausedNode, 'b');
  await eventually(() => controller.nodes.get('a')?.phase === 'idle');
  assert.equal(controller.nodes.get('b')?.phase, 'pending');

  // Nor what a breakpoint does not hold, which runs on beside it.
  controller.stop();
  await eventually(() => controller.mode === 'idle');
  controller.toggleBreakpoint('a');
  const running = controller.run();
  await eventually(() => controller.pausedNode === 'a');
  await eventually(() => controller.nodes.get('b')?.phase === 'idle');
  assert.equal(controller.nodes.get('a')?.phase, 'pending');
  controller.resumeBreakpoint();
  await running;
  assert.equal(controller.mode, 'idle');
});

test('a breakpoint fires when running a graph that has finished', async () => {
  const controller = await loadController([['a', 'echo']]);
  await controller.run();
  assert.equal(controller.mode, 'idle');
  controller.toggleBreakpoint('a');
  const running = controller.run();
  await eventually(() => controller.pausedNode === 'a');
  controller.resumeBreakpoint();
  await running;
  assert.equal(controller.mode, 'idle');
});

test('pausing a run holds what would start next', async () => {
  const controller = await loadController(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['loop', 'a.out', 'a.in', true],
      ['ab', 'a.out', 'b.in'],
    ],
  );
  await controller.injectValue('a', 'in', '1');
  // Adding one each time round, it never settles.
  await controller.injectValue('a', 'add', '1');
  const running = controller.run();
  await eventually(() => controller.mode === 'running');
  controller.pause();
  assert.equal(controller.mode, 'paused');
  await eventually(() => controller.pausedNode !== null);
  await controller.step();
  assert.ok(controller.pausedNode !== null, 'the loop goes on, a node at a time');
  controller.resumeBreakpoint();
  assert.equal(controller.mode, 'running');
  controller.stop();
  await running;
});

test('restarting runs the graph again from the top', async () => {
  const controller = await loadController([['a', 'echo']]);
  await controller.run();
  const started = () => controller.traceEvents.filter((e) => e.event === 'run-started').length;
  const before = started();
  await controller.restart();
  assert.equal(controller.mode, 'idle');
  assert.ok(started() > before, 'it ran again');
});

test('stepping with nothing held, or stepping while running, does no harm', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.toggleBreakpoint('a');
  const running = controller.run();
  await eventually(() => controller.pausedNode === 'a');
  await controller.tick();
  assert.equal(controller.mode, 'paused', 'a tick waits, as one is under way');
  controller.resumeBreakpoint();
  await running;
  assert.deepEqual(
    controller.traceEvents.filter((e) => e.event === 'error'),
    [],
  );
});

/** Makes real workers, counting those made and ended. */
function realWorkers(): { make: () => SessionWorker; made: number; ended: number } {
  const counts = {
    made: 0,
    ended: 0,
    make(): SessionWorker {
      counts.made += 1;
      const { port1, port2 } = new MessageChannel();
      ports.push(port1);
      const worker = new Worker(
        new URL('../../witgraph-loader/test-support/remote-worker.ts', import.meta.url),
        { workerData: { port: port2 }, transferList: [port2] },
      );
      return Object.assign(port1 as unknown as Endpoint, {
        terminate() {
          counts.ended += 1;
          return worker.terminate();
        },
      });
    },
  };
  return counts;
}

test('stop ends a worker whose node spins, and loads the graph in another', async () => {
  const workers = realWorkers();
  const controller = await loadController([['busy', 'busy-loop']], [], { worker: workers.make });
  await controller.injectValue('busy', 'action', 'spin');
  const running = controller.run();
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.equal(controller.mode, 'running');

  await controller.stop();
  await running;
  assert.equal(controller.mode, 'idle', 'it is no longer running');
  assert.deepEqual([workers.made, workers.ended], [2, 1]);
  assert.ok(
    controller.traceEvents.some(
      (e) => e.event === 'warning' && /`busy` did not stop/.test(e.message ?? ''),
    ),
    'and the timeline says why',
  );

  // It kept its inputs, so it spins again, and that can be stopped too.
  const again = controller.run();
  await new Promise((resolve) => setTimeout(resolve, 200));
  await controller.stop();
  await again;
  assert.deepEqual([workers.made, workers.ended], [3, 2]);

  // Told otherwise, the new worker runs it as any other.
  await controller.injectValue('busy', 'action', 'finish');
  await controller.run();
  assert.equal(controller.mode, 'idle');
  assert.equal(await controller.readOutput('busy', 'done'), 1);

  controller.dispose();
  controllers.splice(controllers.indexOf(controller), 1);
  assert.deepEqual([workers.made, workers.ended], [3, 3], 'none is left behind');
});

test('stop leaves a worker that answers alone', async () => {
  const workers = realWorkers();
  const controller = await loadController(
    [
      ['prod', 'stream-producer'],
      ['take', 'stream-consumer'],
    ],
    [['pt', 'prod.items', 'take.items']],
    { worker: workers.make },
  );
  const running = controller.run();
  await new Promise((resolve) => setTimeout(resolve, 200));
  await controller.stop();
  await running;
  assert.equal(controller.mode, 'idle');
  assert.deepEqual([workers.made, workers.ended], [1, 0]);
  controller.dispose();
  controllers.splice(controllers.indexOf(controller), 1);
});

const lineWalkSource = await readFile(
  new URL('test-fixtures/test-components/guests/line-walk/src/lib.rs', root),
  'utf8',
);

/** The line of the `// mark: {name}` comment in `line-walk`'s source. */
function mark(name: string): number {
  const at = lineWalkSource.split('\n').findIndex((line) => line.endsWith(`// mark: ${name}`));
  assert.ok(at >= 0, `no mark \`${name}\``);
  return at + 1;
}

/** A graph of `line-walk` in a real worker, for stopping on its lines. */
async function loadLines(
  connections: Connection[] = [],
  nodes: [string, string][] = [['a', 'line-walk']],
) {
  const workers = realWorkers();
  const controller = await loadController(nodes, connections, {
    worker: workers.make,
    lines: true,
    sources: (file) => (file.endsWith('line-walk/src/lib.rs') ? lineWalkSource : undefined),
  });
  const [file] = controller.lineFiles;
  assert.ok(file?.endsWith('line-walk/src/lib.rs'), `${controller.lineFiles}`);
  return { controller, file, workers };
}

const lineOf = (controller: EditorController) => controller.lineStop?.line;

test('a run stops on a line, shows it, and goes on from it by step or continue', async () => {
  const { controller, file } = await loadLines();
  assert.ok(controller.linesOf(file).includes(mark('walk-first')));
  assert.equal(await controller.sourceOf(file), lineWalkSource);

  controller.toggleLineBreakpoint(file, mark('walk-first'));
  assert.deepEqual(controller.lineBreakpoints(file), [mark('walk-first')]);
  await controller.injectValue('a', 'in', '5');
  const running = controller.run();
  await eventually(() => controller.lineStop !== null);
  assert.equal(controller.mode, 'paused');
  assert.equal(lineOf(controller), mark('walk-first'));
  assert.equal(controller.pausedNode, 'a');
  assert.equal(controller.selectedNode, 'a', 'and its node is shown');

  await controller.step();
  assert.equal(lineOf(controller), mark('walk-second'), 'step goes over the call');
  assert.equal(
    controller.lineStop?.variables.find(({ name }) => name === 'a')?.value,
    '6',
    'and shows what it came to',
  );
  await controller.stepIn();
  assert.match(controller.lineStop?.frames[0] ?? '', /add/);
  await controller.stepOut();
  assert.match(controller.lineStop?.frames[0] ?? '', /walk/);

  controller.resumeBreakpoint();
  assert.equal(controller.lineStop, null);
  await running;
  assert.equal(controller.mode, 'idle');
  assert.equal(await controller.readOutput('a', 'out'), 8);
});

test('a step never stops in what a macro generated', async () => {
  const { controller, file } = await loadLines();
  controller.toggleLineBreakpoint(file, mark('run-walk'));
  const running = controller.run();
  await eventually(() => controller.lineStop !== null);
  controller.toggleLineBreakpoint(file, mark('run-walk'));
  const seen: (number | undefined)[] = [];
  while (controller.lineStop && seen.length < 40) {
    seen.push(lineOf(controller));
    await controller.stepIn();
  }
  assert.ok(seen.includes(mark('add-body')), `${seen}`);
  assert.ok(!seen.includes(1), `the \`generate!\` line: ${seen}`);
  await running;
});

test('stop ends a guest stopped on a line, in time, and its breakpoints stay', async () => {
  const { controller, file, workers } = await loadLines();
  controller.toggleLineBreakpoint(file, mark('walk-first'));
  const running = controller.run();
  await eventually(() => controller.lineStop !== null);

  const started = performance.now();
  await controller.stop();
  await running;
  assert.ok(performance.now() - started < 1500, 'it was not left to be taken for stuck');
  assert.equal(controller.mode, 'idle');
  assert.equal(controller.lineStop, null);
  assert.deepEqual([workers.made, workers.ended], [1, 0], 'the worker answered');

  // The next run stops there again.
  assert.deepEqual(controller.lineBreakpoints(file), [mark('walk-first')]);
  const again = controller.run();
  await eventually(() => controller.lineStop !== null);
  assert.equal(lineOf(controller), mark('walk-first'));
  // Restarting runs on to it too, and is over only when that run is.
  const restarted = controller.restart();
  await eventually(() => controller.lineStop !== null && controller.mode === 'paused');
  await new Promise((resolve) => setTimeout(resolve, 50));
  await eventually(() => controller.lineStop !== null);
  await controller.stop();
  await Promise.all([again, restarted]);
  assert.equal(controller.mode, 'idle');
});

test('pausing stops a guest mid-spin, and stopping it from there ends it', async () => {
  const { controller, workers } = await loadLines();
  await controller.injectValue('a', 'spin', '4000000000');
  const running = controller.run();
  await new Promise((resolve) => setTimeout(resolve, 200));
  controller.pause();
  assert.equal(controller.mode, 'paused');
  await eventually(() => controller.lineStop !== null);
  assert.match(controller.lineStop?.frames[0] ?? '', /spin/);

  // Let go, it spins on with nothing in the way, so it is the worker that goes.
  await controller.stop();
  await running;
  assert.equal(controller.mode, 'idle');
  assert.deepEqual([workers.made, workers.ended], [2, 1]);
});

test('breakpoints stay through a reload, and a step in goes from a node into its code', async () => {
  const { controller, file } = await loadLines();
  controller.toggleLineBreakpoint(file, mark('walk-first'));
  controller.moveNode('a', 10, 10);
  await controller.injectValue('a', 'in', '1');
  controller.renameNode('a', 'renamed');
  await eventually(() => controller.nodes.has('renamed') && controller.lineFiles.length > 0);
  const running = controller.run();
  await eventually(() => controller.lineStop !== null);
  assert.equal(controller.lineStop?.node, 'renamed');
  controller.toggleLineBreakpoint(file, mark('walk-first'));
  controller.resumeBreakpoint();
  await running;

  // Held at a node, stepping in stops at its first line.
  const stepped = controller.step();
  await stepped;
  assert.equal(controller.pausedNode, 'renamed');
  assert.equal(controller.lineStop, null);
  await controller.stepIn();
  assert.equal(controller.lineStop?.node, 'renamed');
  await controller.stop();
});

test('with no debug info or no way to stop, a graph has no lines, and runs', async () => {
  const none = await loadController([['e', 'echo']], [], { lines: true });
  assert.deepEqual(none.lineFiles, []);
  await none.run();
  assert.equal(none.mode, 'idle');

  const scope = globalThis as { crossOriginIsolated?: boolean };
  scope.crossOriginIsolated = false;
  try {
    const controller = await loadController([['e', 'echo']], [], { lines: true });
    assert.deepEqual(controller.lineFiles, []);
    assert.ok(
      controller.traceEvents.some(
        (e) => e.event === 'warning' && /shared memory/.test(e.message ?? ''),
      ),
      'and the timeline says why',
    );
  } finally {
    delete scope.crossOriginIsolated;
  }
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
  await controller.injectValue('a', 'in', '5');
  await controller.run();
  assert.ok(controller.traceEvents.length > 0, 'should have trace events after a run');
});

// ---------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------

/** `wasi:clocks/monotonic-clock`, as the WASI shim provides it. */
const clock = (capability: Capability) =>
  capability.interface === 'wasi:clocks/monotonic-clock@0.3.2' ? monotonicClock : undefined;

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
  await controller.injectValue('source', 'frames', '3');
  await controller.run();
  await pause(50);
  assert.ok(changes >= 1, 'stream items are announced');
  assert.equal(await controller.readOutput('sink', 'count'), 3);

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

test('a graph is not edited while it runs, until the run stops', async () => {
  const controller = await loadVideo();
  // Endless, paced: a run lasts until it is stopped.
  await controller.injectValue('source', 'fps', '200');
  assert.equal(controller.editable, true);

  const running = controller.run();
  while (controller.mode !== 'running') await pause(5);
  assert.equal(controller.editable, false);

  // What would load the graph again waits, and the trace says why.
  const before = controller.nodes.size;
  controller.removeNode('sink');
  controller.renameNode('gray', 'grey');
  controller.undo();
  await controller.reset();
  assert.equal(controller.nodes.size, before);
  assert.ok(controller.nodes.has('sink'));
  assert.ok(controller.nodes.has('gray'));
  assert.equal(controller.mode, 'running', 'the run goes on');
  const warnings = controller.traceEvents.filter(
    (e) => e.event === 'warning' && e.message?.includes('cannot be edited while it runs'),
  );
  assert.equal(warnings.length, 1, 'said once, however many edits');
  // What does not load it again does not wait: moving a node, say.
  controller.moveNode('gray', 321, 123);
  assert.equal(controller.nodes.get('gray')?.x, 321);

  controller.stop();
  await running;
  assert.equal(controller.editable, true);
  controller.removeNode('sink');
  assert.equal(controller.nodes.has('sink'), false);
});

test('stop ends a stream still being written, and run starts it again', async () => {
  const controller = await loadVideo();
  // Endless, paced: a run lasts until it is stopped.
  await controller.injectValue('source', 'fps', '200');
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
  await controller.injectValue('prod', 'burst-size', '5');
  await controller.tick();
  const items = controller.observe('prod', 'items');
  assert.deepEqual([items.kind, items.count, items.latest], ['stream', 5, 4]);
  assert.deepEqual(items.recent, [0, 1, 2, 3, 4]);

  await controller.injectValue('prod', 'burst-size', '3');
  await controller.tick();
  const more = controller.observe('prod', 'items');
  assert.equal(more.count, 8, 'counted across runs');
  assert.deepEqual(more.recent, [0, 1, 2, 3, 4, 0, 1, 2]);

  // Only the newest are kept; all are counted.
  await controller.injectValue('prod', 'burst-size', '500');
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
  await controller.tick();
  for (let i = 0; i < 50 && controller.observe('w', 'done').count === 0; i++) await pause(10);
  const done = controller.observe('w', 'done');
  assert.deepEqual([done.latest, done.count, done.pending], [7, 1, false]);
  assert.equal(await controller.readOutput('r', 'value'), 7);
});

test('a Value output keeps each new value', async () => {
  const controller = await loadController([['n', 'relay']], [['loop', 'n.out', 'n.in', true]]);
  assert.equal(controller.nodes.get('n')?.preview?.viewer, 'series');
  await controller.injectValue('n', 'in', '1');
  await controller.injectValue('n', 'add', '2');
  for (let i = 0; i < 3; i++) await controller.tick();
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
  await controller.injectValue('n', 'in', '0');
  await controller.injectValue('n', 'add', '1');
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
  await controller.injectValue('source', 'fps', '100');
  const started = performance.now();
  await controller.tick();
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
  await controller.injectValue('n', 'in', '0');
  await controller.injectValue('n', 'add', '1');
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
  await controller.injectValue('n', 'in', '0');
  await controller.injectValue('n', 'add', '1');
  controller.setPreview('n', 'none');
  controller.select(null);
  assert.equal(controller.isObserved('n', 'out'), false);
  await controller.tick();
  await controller.tick();
  assert.equal(controller.observe('n', 'out').count, 0, 'nothing recorded');
  // Its latest value is still read when asked for.
  assert.equal(await controller.readOutput('n', 'out'), 2);

  // Selected, the inspector shows it: it is recorded from then on.
  controller.select('n');
  assert.equal(controller.isObserved('n', 'out'), true);
  await pause(0);
  assert.equal(controller.observe('n', 'out').count, 1, 'what it holds now');
  await controller.tick();
  assert.equal(controller.observe('n', 'out').count, 2);
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
    capabilities: (capability: Capability) =>
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
  for (let i = 0; i < 20; i++) await controller.tick();
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
  assert.match(
    controller.canConnect('src', 'tensors', 'sink', 'tensors').reason ?? '',
    /already|duplicat|one reader|fan|writer/i,
    'the types fit, but a stream has one reader and `sink` has a writer',
  );
  assert.doesNotMatch(
    controller.canConnect('src', 'tensors', 'sink', 'tensors').reason ?? '',
    /mixes/,
  );
  assert.match(
    controller.canConnect('one', 'tensor', 'sink', 'tensors').reason ?? '',
    /mixes port kinds/,
    'a handle port is not a stream',
  );
  assert.match(
    controller.canConnect('fut', 'tensor', 'sink', 'tensors').reason ?? '',
    /mixes port kinds/,
    'nor is a future',
  );
  assert.match(
    controller.canConnect('nums', 'items', 'sink', 'tensors').reason ?? '',
    /mixes payload types/,
    'a stream of numbers is not a stream of tensors',
  );
  assert.equal(controller.canConnect('src', 'tensors', 'take', 'items').ok, false);
  assert.deepEqual(
    controller.compatibleOutputsForInput('sink', 'tensors').map((o) => o.node),
    ['inv'],
    '`src` fits by type, but it feeds `inv`, and a stream has one reader',
  );
  const asFeedback = controller.canBeFeedback('src', 'tensors', 'inv', 'tensors');
  assert.equal(asFeedback.ok, false);
  assert.match(asFeedback.reason ?? '', /carries a stream port/);
});

test('a preview shows what a view read of a stream of handles, and keeps none', async () => {
  const tensors = cpuTensors();
  const kept: unknown[] = [];
  registerViewer({ id: 'tensor', mount: () => ({ update() {}, dispose() {} }) });
  const controller = await loadController(...streamOfTensors, {
    capabilities: tensors.capabilities,
    views: {
      tensor: {
        keep(item) {
          const reference = (item as { clone(): { label: string } }).clone();
          kept.push(reference);
          return reference;
        },
        read: (reference) => ({ label: (reference as { label: string }).label }),
      },
    },
  });
  await controller.injectValue('src', 'frames', '4');
  await settled(controller);
  await eventually(() => controller.observe('src', 'tensors').count === 4);
  const view = controller.observe('src', 'tensors');
  assert.ok(kept.length >= 4, 'each node previewing its stream keeps its own');
  assert.deepEqual(view.latest, { label: 'kept' });
  assert.deepEqual(view.recent, [view.latest], 'no history holding on to them');
  // What a view keeps is gone once read: nothing waits for a preview to let go of it.
  await eventually(() => tensors.live.size === 0);
});

test('with no view of them, a preview describes a stream of handles', async () => {
  const tensors = cpuTensors();
  registerViewer({ id: 'tensor', mount: () => ({ update() {}, dispose() {} }) });
  const controller = await loadController(...streamOfTensors, {
    capabilities: tensors.capabilities,
  });
  await controller.injectValue('src', 'frames', '2');
  await settled(controller);
  const view = controller.observe('src', 'tensors');
  assert.equal(view.count, 2);
  assert.equal(view.latest, '<tensor>');
  assert.equal(tensors.live.size, 0, 'it holds on to none');
});

test('a future of a handle is shown like a stream item', async () => {
  const tensors = cpuTensors();
  registerViewer({ id: 'tensor', mount: () => ({ update() {}, dispose() {} }) });
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

/** Waits for what an edit reloads: `ready` says when it has. */
async function until(ready: () => boolean): Promise<void> {
  for (let at = 0; at < 500 && !ready(); at++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.ok(ready(), 'the edit took too long');
}

test('a connection is refused by the rules a load goes by', async () => {
  const controller = await loadController(
    [
      ['e', 'echo'],
      ['f', 'echo'],
      ['r', 'relay'],
      ['s', 'relay'],
    ],
    [
      ['ef', 'e.out', 'f.in'],
      ['rs', 'r.out', 's.in'],
      ['rr', 'r.out', 'r.in', true],
    ],
  );
  const refusal = (from: string, out: string, to: string, input: string) =>
    controller.canConnect(from, out, to, input).reason ?? '';
  assert.deepEqual(controller.canConnect('f', 'out', 'e', 'in'), { ok: true });
  assert.match(refusal('e', 'out', 'r', 'add'), /mixes payload types/, 'f64 into u32');
  assert.match(refusal('e', 'out', 'f', 'in'), /duplicates the endpoints of `ef`/);
  assert.match(refusal('f', 'out', 'f', 'in'), /writer/, 'an input has one writer');
  assert.match(refusal('e', 'nope', 'f', 'in'), /nope/);
  assert.match(refusal('nope', 'out', 'f', 'in'), /nope/);
  assert.deepEqual(
    controller.compatibleOutputsForInput('f', 'in').map((o) => o.node),
    ['e', 'f'],
    'connecting here replaces what is there, and the types decide the rest',
  );
});

test('what compiling says of a graph is on what it is about', async () => {
  const controller = await loadController(
    [
      ['a', 'echo'],
      ['b', 'echo'],
      ['c', 'echo'],
    ],
    [['ab', 'a.out', 'b.in']],
  );
  assert.deepEqual(controller.compileDiagnostics, []);

  // A cycle is for the next edit to mend, so it is not refused.
  assert.equal(controller.canConnect('b', 'out', 'a', 'in').ok, true);
  controller.addConnection('ba', { node: 'b', port: 'out' }, { node: 'a', port: 'in' });
  await until(() => controller.compileDiagnostics.length > 0);
  const [cycle] = controller.compileDiagnostics;
  assert.equal(cycle.code, 'witgraph::ir::illegal_cycle');
  assert.equal(controller.compiles, false);
  assert.deepEqual(controller.diagnosticsForNode('a'), [cycle]);
  assert.deepEqual(controller.diagnosticsForNode('b'), [cycle]);
  assert.deepEqual(controller.diagnosticsForNode('c'), []);

  controller.setConnectionFeedback('ba', true);
  await until(() => controller.compileDiagnostics.length === 0);
  assert.equal(controller.compiles, true);
});

test('a warning is said of a graph that compiles, and is not why one does not', async () => {
  const controller = await loadController(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['ab', 'a.out', 'b.in'],
      ['loop', 'a.out', 'a.in', true],
    ],
  );
  const codes = () => controller.compileDiagnostics.map((d) => d.code);
  assert.equal(controller.compiles, true);
  assert.deepEqual(codes(), ['witgraph::ir::feedback_only_input']);
  const [warning] = controller.diagnosticsForNode('a');
  assert.equal(warning.severity, 'warning');
  assert.deepEqual(controller.diagnosticsForNode('b'), []);

  controller.addConnection('ba', { node: 'b', port: 'out' }, { node: 'a', port: 'add' });
  await until(() => controller.compileError !== null);
  assert.deepEqual(codes(), ['witgraph::ir::feedback_only_input', 'witgraph::ir::illegal_cycle']);
  assert.doesNotMatch(controller.compileError ?? '', /fed only by feedback/);

  controller.removeConnection('ba');
  await until(() => controller.compiles);
  assert.deepEqual(codes(), ['witgraph::ir::feedback_only_input'], 'the warning stays');

  // Injecting a value is what the warning asks for: it is settled, and still said.
  const settled = () => controller.diagnosticsForNode('a').map((d) => controller.isSettled(d));
  await controller.injectValue('a', 'in', '1');
  assert.deepEqual(codes(), ['witgraph::ir::feedback_only_input']);
  assert.deepEqual(settled(), [true]);
  await controller.clearInput('a', 'in');
  assert.deepEqual(settled(), [false]);

  // An attribute is only a request: one that fails to inject settles nothing.
  const withInject = (value: string) => {
    const graph = JSON.parse(controller.exportGraph()) as GraphJson;
    const node = graph.nodes.find((n) => n.id === 'a');
    if (node) node.attrs = { ...node.attrs, 'inject.in': value };
    return JSON.stringify(graph);
  };
  await controller.importGraph(withInject('"not a number"'));
  assert.equal(controller.compiles, true);
  assert.deepEqual(settled(), [false]);
  await controller.importGraph(withInject('7'));
  assert.deepEqual(settled(), [true]);
});

test('what can feed an input is worked out once per graph', async () => {
  const controller = await loadController(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['ab', 'a.out', 'b.in'],
      ['loop', 'a.out', 'a.in', true],
    ],
  );
  const feeds = () => controller.compatibleOutputsForInput('b', 'add');
  const before = feeds();
  assert.equal(feeds(), before, 'nothing changed, so nothing is compiled');
  assert.deepEqual(
    before.map((o) => o.label),
    ['a.out', 'b.out'],
  );

  controller.setNodeLabel('a', 'First');
  assert.deepEqual(
    feeds().map((o) => o.label),
    ['First.out', 'b.out'],
    'a change of the graph is seen at once',
  );
});

test('a refused connection costs an input nothing', async () => {
  const controller = await loadController(
    [
      ['a', 'relay'],
      ['b', 'relay'],
      ['e', 'echo'],
    ],
    [
      ['ab', 'a.out', 'b.in'],
      ['loop', 'a.out', 'a.in', true],
    ],
  );
  controller.connectInput('b', 'in', 'e', 'out');
  assert.equal(controller.connectionForInput('b', 'in')?.id, 'ab');
});

test('feedback on what cannot have it is downgraded, not refused', async () => {
  const controller = await loadController(
    [
      ['p', 'stream-producer'],
      ['q', 'stream-producer'],
      ['c', 'stream-consumer'],
    ],
    [['pc', 'p.items', 'c.items']],
  );
  controller.connectInput('c', 'items', 'q', 'items', true);
  const connection = controller.connectionForInput('c', 'items');
  assert.equal(connection?.fromNode, 'q');
  assert.equal(connection?.feedback, false);
});

test('feedback goes by the rules a load goes by: a handle can have it, a stream cannot', async () => {
  const tensors = cpuTensors();
  const controller = await loadController(
    [
      ['source', 'tensor-source'],
      ['invert', 'tensor-invert'],
      ['nums', 'stream-producer'],
      ['take', 'stream-consumer'],
    ],
    [
      ['handle', 'source.tensor', 'invert.tensor'],
      ['stream', 'nums.items', 'take.items'],
    ],
    { capabilities: tensors.capabilities },
  );
  assert.deepEqual(controller.canBeFeedback('source', 'tensor', 'invert', 'tensor'), { ok: true });

  const errors: string[] = [];
  controller.addEventListener('trace-event', () => {
    for (const { event, message } of controller.traceEvents)
      if (event === 'error' && message) errors.push(message);
  });
  controller.setConnectionFeedback('stream', true);
  assert.equal(controller.connectionForInput('take', 'items')?.feedback, false);
  assert.ok(
    errors.some((e) => /carries a stream port; .* values or handles/.test(e)),
    `the compiler's reason, not a copy of it: ${errors}`,
  );

  controller.setConnectionFeedback('handle', true);
  assert.equal(controller.connectionForInput('invert', 'tensor')?.feedback, true);
});

test('after a step, phases say what is still running, until it has ended', async () => {
  const controller = await loadController(
    [
      ['prod', 'stream-producer'],
      ['pass', 'stream-relay'],
      ['drain', 'stream-drain'],
    ],
    [
      ['a', 'prod.items', 'pass.items'],
      ['b', 'pass.items', 'drain.items'],
    ],
  );
  const phases = () => [...controller.nodes.values()].map((node) => node.phase);
  // Long enough to outlast a step, which is one short tick.
  await controller.injectValue('prod', 'burst-size', '2000');
  await controller.tick();
  assert.equal(controller.mode, 'idle', 'the step is over');
  assert.deepEqual(phases(), ['running', 'running', 'running'], 'the stream is not');
  const ticks = controller.tickCount;

  await until(() => phases().every((phase) => phase === 'idle'));
  assert.equal(controller.tickCount, ticks, 'with no tick, so with nothing new started');
});
