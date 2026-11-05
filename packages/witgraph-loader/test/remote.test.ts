// A session reached over a message channel: in this thread, where a test can see both
// sides, and over a real worker, for what only another thread shows.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { after, afterEach, test } from 'node:test';
import { pathToFileURL } from 'node:url';
import { MessageChannel, Worker } from 'node:worker_threads';
import {
  type Capability,
  diagnosticsOf,
  type Endpoint,
  type HandleView,
  RemoteSession,
  type RemoteSessionOptions,
  type ServeOptions,
  serveSession,
} from '../src/index.ts';

async function guestDir(): Promise<URL> {
  const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: new URL('../../..', import.meta.url),
    encoding: 'utf8',
  });
  const build = new URL('debug/build/', pathToFileURL(`${JSON.parse(metadata).target_directory}/`));
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
const wasm = await readFile(new URL('../pkg/witgraph_web_bg.wasm', import.meta.url));

type Connection = [id: string, from: string, to: string, feedback?: boolean];

const ref = (guest: string) => ({
  package: { namespace: 'test', name: guest, version: '0.1.0' },
  world: guest,
});

async function options(
  nodes: [string, string][],
  connections: Connection[] = [],
  more: Partial<RemoteSessionOptions> = {},
): Promise<RemoteSessionOptions> {
  const used = [...new Set(nodes.map(([, guest]) => guest))];
  const port = (text: string) => {
    const [node, name] = text.split('.');
    return { node, port: name };
  };
  return {
    graph: {
      metadata: { name: 'remote' },
      components: used.map(ref),
      nodes: nodes.map(([id, guest]) => ({ id, component: ref(guest) })),
      connections: connections.map(([id, from, to, feedback]) => ({
        id,
        from: port(from),
        to: port(to),
        ...(feedback ? { feedback: true } : {}),
      })),
    },
    components: await Promise.all(
      used.map(async (guest) => ({
        id: `test:${guest}/${guest}@0.1.0`,
        bytes: new Uint8Array(await readFile(new URL(`${guest}.wasm`, guests))),
      })),
    ),
    wasm,
    ...more,
  };
}

const sessions: RemoteSession[] = [];
const ports: { close(): void }[] = [];
afterEach(() => {
  for (const session of sessions.splice(0)) session.dispose();
});
after(() => {
  for (const port of ports.splice(0)) port.close();
});

/** Serves over a channel in this thread, and loads there. */
async function load(
  nodes: [string, string][],
  connections: Connection[] = [],
  more: Partial<RemoteSessionOptions> = {},
  serve: ServeOptions = {},
): Promise<RemoteSession> {
  const { port1, port2 } = new MessageChannel();
  ports.push(port1, port2);
  serveSession(port2 as unknown as Endpoint, serve);
  const session = await RemoteSession.load(
    port1 as unknown as Endpoint,
    await options(nodes, connections, more),
  );
  sessions.push(session);
  return session;
}

async function until(ready: () => boolean, what = 'it never happened'): Promise<void> {
  for (let at = 0; at < 600 && !ready(); at++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.ok(ready(), what);
}

test('a session across a channel loads, ticks, and reads as one here does', async () => {
  const session = await load(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [['ab', 'a.out', 'b.in']],
    {},
  ).catch(() => undefined);
  // `a.in` is required and unconnected: that is the graph's fault.
  assert.equal(session, undefined);

  const looped = await load(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['loop', 'a.out', 'a.in', true],
      ['ab', 'a.out', 'b.in'],
    ],
  );
  assert.deepEqual([...looped.contracts.keys()].sort(), ['a', 'b']);
  assert.equal(looped.contracts.get('a')?.outputs[0].name, 'out');
  await looped.graph.inject('a', 'in', 5);
  assert.deepEqual(await looped.tick(), { result: 'progress' });
  assert.equal(await looped.graph.readOutput('b', 'out'), 5);
  assert.equal(await looped.graph.readOutputWave('b', 'out'), '5');
  assert.deepEqual(await looped.readInputs('b'), { in: 5, add: undefined });
  await looped.settle();
  assert.equal((looped.graph.nodeState('b') as { phase: string }).phase, 'idle');
  assert.ok(looped.graph.outputVersion('b', 'out') > 0);
  assert.throws(() => looped.graph.nodeState('nobody'), /no node/);

  const restored = await looped.graph.snapshot();
  await looped.graph.restore(restored);
});

test('what stops a graph compiling crosses with its diagnostics', async () => {
  await assert.rejects(load([['a', 'relay']]), (error: unknown) => {
    const found = diagnosticsOf(error);
    assert.equal(found?.[0].code, 'witgraph::ir::required_input_unconnected');
    assert.deepEqual(found?.[0].location.nodes, ['a']);
    return true;
  });
});

test('only the runs named wait for the page, and the rest go on', async () => {
  const told: string[] = [];
  const release: Record<string, () => void> = {};
  const session = await load(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [],
    {
      holds: ['b'],
      beforeRun: (node, held) => {
        told.push(`${node}:${held}`);
        if (held)
          return new Promise((resolve) => {
            release[node] = resolve;
          });
      },
      afterRun: (node) => told.push(`${node}:returned`),
    },
  );
  const tick = session.tick();
  await until(() => release.b !== undefined, '`b` was never held');
  await until(() => told.includes('a:returned'), '`a` waited for `b`');
  assert.deepEqual(
    told.filter((t) => t.startsWith('b')),
    ['b:true'],
  );
  release.b();
  await tick;
  await session.settle();
  assert.ok(told.includes('b:returned'));

  // Changing whom it holds applies from the next run.
  told.length = 0;
  delete release.b;
  session.holdAt('all');
  session.graph.rerun('a');
  const next = session.tick();
  await until(() => release.a !== undefined);
  assert.deepEqual(told, ['a:true']);
  release.a();
  await next;
  await session.settle();
});

test('disposing lets go of a run still held', async () => {
  const session = await load([['a', 'echo']], [], {
    holds: 'all',
    beforeRun: () => new Promise(() => {}),
  });
  const tick = session.tick();
  await new Promise((resolve) => setTimeout(resolve, 20));
  session.dispose();
  await tick.catch(() => {});
  await assert.rejects(session.tick(), /disposed/);
  await assert.rejects(session.graph.readOutput('a', 'out'), /disposed/);
});

test("a mock runs in the page, and its failure is the node's", async () => {
  const seen: unknown[] = [];
  const session = await load(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['loop', 'a.out', 'a.in', true],
      ['ab', 'a.out', 'b.in'],
    ],
    {
      mocks: {
        a: (inputs) => {
          seen.push(inputs);
          return { out: 41 };
        },
      },
    },
  );
  await session.graph.inject('a', 'in', 1);
  await session.settle();
  assert.equal(await session.graph.readOutput('b', 'out'), 41);
  assert.ok(seen.length > 0);

  const failing = await load(
    [
      ['a', 'relay'],
      ['b', 'relay'],
    ],
    [
      ['loop', 'a.out', 'a.in', true],
      ['ab', 'a.out', 'b.in'],
    ],
    {
      mocks: {
        a: () => {
          throw new Error('the mock says no');
        },
      },
    },
  );
  await failing.graph.inject('a', 'in', 1);
  const result = await failing.tick();
  assert.equal(result.result, 'faulted');
  assert.match(JSON.stringify(result), /the mock says no/);
});

test('only what is observed crosses, and the state says what ran', async () => {
  const items: unknown[] = [];
  const futures: unknown[] = [];
  const session = await load(
    [
      ['p', 'stream-producer'],
      ['c', 'stream-consumer'],
    ],
    [['pc', 'p.items', 'c.items']],
    { onStreamItem: ({ items: got }) => items.push(...Array.from(got)) },
  );
  await session.graph.inject('p', 'burst-size', 3);
  await session.settle();
  assert.deepEqual(items, [], 'nobody asked');
  assert.ok(session.graph.takeTrace().length > 0, 'the trace is reported');
  assert.deepEqual(session.graph.takeTrace(), [], 'and taken once');

  session.observeOnly([['p', 'items']]);
  session.graph.rerun('p');
  await session.settle();
  assert.deepEqual(items, [0, 1, 2]);
  assert.deepEqual(futures, []);
});

test('a view shows a stream of handles to the page, newest first, and frees them', async () => {
  const live = new Set<object>();
  let gate: () => void = () => {};
  let reads = 0;
  class Tensor {
    constructor() {
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
      return new Tensor();
    }
    async invert() {
      return new Tensor();
    }
    [Symbol.dispose]() {
      live.delete(this);
    }
  }
  const tensor: HandleView<Tensor> = {
    keep: (handle) => (handle as Tensor).clone(),
    read: async () => {
      reads += 1;
      // The first read is slow, so what follows it must wait.
      if (reads === 1)
        await new Promise<void>((resolve) => {
          gate = resolve;
        });
      return { reads };
    },
  };
  const shown: { shown: unknown; count: number }[] = [];
  const session = await load(
    [
      ['src', 'tensor-stream-source'],
      ['sink', 'tensor-stream-sink'],
    ],
    [['a', 'src.tensors', 'sink.tensors']],
    {
      observed: [['src', 'tensors']],
      onHandle: ({ shown: what, count }) => shown.push({ shown: what, count }),
    },
    {
      capabilities: (capability: Capability) =>
        capability.interface === 'test:data/tensor@0.1.0' ? { Tensor } : undefined,
      views: { tensor },
    },
  );
  await session.graph.inject('src', 'frames', 4);
  const settling = session.settle();
  await until(() => reads === 1);
  await settling;
  gate();
  await until(() => shown.reduce((sum, { count }) => sum + count, 0) === 4, 'an item went unsaid');
  assert.equal(shown.length, 2, 'the first, then the newest of those it held up');
  assert.deepEqual(shown[0], { shown: { reads: 1 }, count: 1 });
  assert.deepEqual(shown[1], { shown: { reads: 2 }, count: 3 });
  await session.graph.sync();
  assert.equal(live.size, 0, 'nothing kept is left');
});

test('with no view of it, a handle is described', async () => {
  class Tensor {
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
      return new Tensor();
    }
    async invert() {
      return new Tensor();
    }
  }
  const shown: unknown[] = [];
  const session = await load(
    [
      ['src', 'tensor-stream-source'],
      ['sink', 'tensor-stream-sink'],
    ],
    [['a', 'src.tensors', 'sink.tensors']],
    {
      observed: [['src', 'tensors']],
      onHandle: ({ shown: what }) => shown.push(what),
    },
    {
      capabilities: (capability: Capability) =>
        capability.interface === 'test:data/tensor@0.1.0' ? { Tensor } : undefined,
    },
  );
  await session.graph.inject('src', 'frames', 2);
  await session.settle();
  assert.deepEqual(shown, ['<tensor>', '<tensor>']);
});

test('a handle output is shown through its view, and what cannot cross rejects', async () => {
  class Tensor {
    readonly #shape: number[];
    readonly #data: Uint8Array;
    constructor(shape: ArrayLike<number>, data: Uint8Array) {
      this.#shape = Array.from(shape);
      this.#data = data;
    }
    static async fromBytes(_dtype: string, shape: ArrayLike<number>, data: Uint8Array) {
      return new Tensor(shape, data.slice());
    }
    dtype() {
      return 'uint8';
    }
    shape() {
      return this.#shape;
    }
    async read(offset: bigint, len: bigint) {
      return this.#data.slice(Number(offset), Number(offset + len));
    }
    clone() {
      return new Tensor(this.#shape, this.#data);
    }
    async invert() {
      return this.clone();
    }
  }
  const capabilities = (capability: Capability) =>
    capability.interface === 'test:data/tensor@0.1.0' ? { Tensor } : undefined;
  const nodes: [string, string][] = [
    ['one', 'tensor-source'],
    ['sink', 'tensor-sink'],
  ];
  const connections: Connection[] = [['a', 'one.tensor', 'sink.tensor']];
  const load2 = (views: ServeOptions['views']) =>
    load(nodes, connections, {}, { capabilities, views });

  const plain = await load2({});
  await plain.settle();
  assert.equal(await plain.graph.readHandle('one', 'tensor'), '<tensor>');

  const viewer = plain.contracts.get('one')?.outputs[0].viewer;
  assert.ok(viewer, 'this guest names a viewer');
  const viewed = await load2({
    [viewer]: { keep: (handle) => handle, read: () => ({ shown: true }) },
  });
  await viewed.settle();
  assert.deepEqual(await viewed.graph.readHandle('one', 'tensor'), { shown: true });

  const uncloneable = await load2({
    [viewer]: { keep: (handle) => handle, read: () => () => {} },
  });
  await uncloneable.settle();
  await assert.rejects(uncloneable.graph.readHandle('one', 'tensor'), /could not be cloned/);
});

test('stop ends a tick, and shutdown and rerun keep their order', async () => {
  const session = await load(
    [
      ['p', 'stream-producer'],
      ['c', 'stream-consumer'],
    ],
    [['pc', 'p.items', 'c.items']],
  );
  // Endless, as nothing bounds it.
  let stop: () => void = () => {};
  const tick = await session.tick(
    new Promise<void>((resolve) => {
      stop = resolve;
      setTimeout(resolve, 30);
    }),
  );
  assert.ok(['progress', 'interrupted'].includes(tick.result), tick.result);
  stop();
  const phase = (node: string) => (session.graph.nodeState(node) as { phase: string }).phase;
  assert.equal(phase('p'), 'running');

  session.graph.shutdown();
  await session.graph.sync();
  assert.equal(phase('p'), 'cancelled');

  session.graph.rerun('c');
  session.graph.rerun('p');
  await session.graph.inject('p', 'burst-size', 2);
  await session.settle();
  assert.equal(phase('p'), 'idle');
});

test('a node spinning in a worker leaves this thread free, and the worker can be ended', async () => {
  const { port1, port2 } = new MessageChannel();
  ports.push(port1);
  const worker = new Worker(new URL('../test-support/remote-worker.ts', import.meta.url), {
    workerData: { port: port2 },
    transferList: [port2],
  });
  try {
    const session = await RemoteSession.load(
      port1 as unknown as Endpoint,
      await options([['busy', 'busy-loop']]),
    );
    await session.graph.injectWave('busy', 'action', 'spin');
    let beats = 0;
    const beat = setInterval(() => beats++, 5);
    let ended = false;
    void session.tick().then(
      () => {
        ended = true;
      },
      () => {
        ended = true;
      },
    );
    await new Promise((resolve) => setTimeout(resolve, 400));
    clearInterval(beat);
    assert.ok(beats >= 30, `this thread ran only ${beats} times`);
    assert.equal(ended, false, 'the tick spins on');
  } finally {
    await worker.terminate();
  }
});
