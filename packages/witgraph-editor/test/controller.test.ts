// Integration tests for EditorController using the actual witgraph-loader
// with test component fixtures. Build the guests first:
//   cargo build -p test-components
//   cd packages/witgraph-loader && npm run build:wasm

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFile, readdir, stat } from 'node:fs/promises';
import { afterEach, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { EditorController } from '../src/controller/editor-controller.ts';
import type { GraphJson } from '../src/controller/types.ts';
import type { ComponentSource } from 'witgraph-loader';

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
  await controller.load({ graph, components, wasm });
  controllers.push(controller);
  return controller;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test('load and dispose', async () => {
  const controller = await loadController(
    [['a', 'echo'], ['b', 'echo']],
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
    [['a', 'echo'], ['b', 'echo']],
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
  assert.ok(changed, 'state-changed event should have fired');
});

test('autoLayout positions nodes', async () => {
  const controller = await loadController(
    [['a', 'echo'], ['b', 'echo']],
    [['c', 'a.out', 'b.in']],
  );
  await controller.autoLayout();

  const posA = controller.nodes.get('a')!;
  const posB = controller.nodes.get('b')!;
  assert.ok(posA.x >= 0, 'a.x non-negative');
  assert.ok(posA.y >= 0, 'a.y non-negative');
  assert.ok(posB.x >= 0, 'b.x non-negative');
  assert.ok(posB.y >= 0, 'b.y non-negative');
  assert.ok(
    posA.x !== posB.x || posA.y !== posB.y,
    'nodes should not share the same position',
  );
});

test('trace events are collected', async () => {
  const controller = await loadController([['a', 'echo']]);
  controller.injectValue('a', 'in', '5');
  await controller.run();
  assert.ok(controller.traceEvents.length > 0, 'should have trace events after a run');
});
