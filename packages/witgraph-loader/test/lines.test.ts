// Stopping guests on source lines, from another thread: a stop blocks the one a guest is on.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { afterEach, test } from 'node:test';
import { pathToFileURL } from 'node:url';
import { MessageChannel, Worker } from 'node:worker_threads';
import {
  type Endpoint,
  endWorker,
  type LineDebugger,
  type LineStop,
  RemoteSession,
  type RemoteSessionOptions,
  Session,
  type SessionWorker,
  serveSession,
  type TickResult,
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
    const guest = await stat(new URL('line-walk.wasm', out)).catch(() => undefined);
    if (guest) candidates.push([guest.mtimeMs, out]);
  }
  candidates.sort((a, b) => b[0] - a[0]);
  assert.ok(candidates.length > 0, 'build the guests with `cargo build -p test-components`');
  return candidates[0][1];
}

const guests = await guestDir();
const wasm = await readFile(new URL('../pkg/witgraph_web_bg.wasm', import.meta.url));
const source = await readFile(
  new URL('../../../test-fixtures/test-components/guests/line-walk/src/lib.rs', import.meta.url),
  'utf8',
);

/** The line of the `// mark: {name}` comment in `line-walk`'s source. */
function mark(name: string): number {
  const at = source.split('\n').findIndex((line) => line.endsWith(`// mark: ${name}`));
  assert.ok(at >= 0, `no mark \`${name}\``);
  return at + 1;
}

const ref = (guest: string) => ({
  package: { namespace: 'test', name: guest, version: '0.1.0' },
  world: guest,
});

async function options(
  nodes: [string, string][],
  connections: [string, string, string, boolean?][] = [],
  more: Partial<RemoteSessionOptions> = {},
): Promise<RemoteSessionOptions> {
  const used = [...new Set(nodes.map(([, guest]) => guest))];
  const port = (text: string) => {
    const [node, name] = text.split('.');
    return { node, port: name };
  };
  return {
    graph: {
      metadata: { name: 'lines' },
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

const workers: { terminate(): unknown }[] = [];
afterEach(async () => {
  for (const worker of workers.splice(0)) await worker.terminate();
});

function realWorker(): SessionWorker {
  const { port1, port2 } = new MessageChannel();
  const worker = new Worker(new URL('../test-support/remote-worker.ts', import.meta.url), {
    workerData: { port: port2 },
    transferList: [port2],
  });
  workers.push({
    terminate() {
      port1.close();
      return worker.terminate();
    },
  });
  return Object.assign(port1 as unknown as Endpoint, { terminate: () => worker.terminate() });
}

/** A session in a real worker, and its stops as they come. */
async function load(
  nodes: [string, string][],
  connections: [string, string, string, boolean?][] = [],
  more: Partial<RemoteSessionOptions> = {},
  which: { all?: boolean; own?: readonly string[] } = {},
) {
  const stops: LineStop[] = [];
  const waiting: ((stop: LineStop) => void)[] = [];
  const worker = realWorker();
  const session = await RemoteSession.load(
    worker,
    await options(nodes, connections, {
      ...more,
      lines: {
        ...which,
        onStop(stop) {
          const next = waiting.shift();
          if (next) next(stop);
          else stops.push(stop);
        },
      },
    }),
  );
  const lines = session.lines as LineDebugger;
  const nextStop = (): Promise<LineStop> => {
    const stop = stops.shift();
    if (stop) return Promise.resolve(stop);
    return new Promise((resolve) => waiting.push(resolve));
  };
  const file = lines.files.find((name) => name.endsWith('guests/line-walk/src/lib.rs')) as string;
  return { session, lines, nextStop, file, worker };
}

test('a breakpoint stops on its line, naming the node, and resuming goes on', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  assert.ok(file, 'the guest has its own lines');
  assert.equal(lines.setBreakpoint(file, mark('walk-first')), mark('walk-first'));
  await session.graph.inject('a', 'in', 5);

  const tick = session.tick();
  const stop = await nextStop();
  assert.equal(stop.reason, 'breakpoint');
  assert.equal(stop.file, file);
  assert.equal(stop.line, mark('walk-first'));
  assert.equal(stop.node, 'a');
  assert.equal(stop.frames[0].function, 'line_walk_guest::walk');
  assert.deepEqual(lines.stopped?.line, stop.line);

  // This thread goes on, for as long as the guest is held.
  let beats = 0;
  const beat = setInterval(() => beats++, 5);
  await new Promise((resolve) => setTimeout(resolve, 100));
  clearInterval(beat);
  assert.ok(beats > 5);

  lines.resume();
  assert.equal(lines.stopped, undefined);
  await tick;
  await session.settle();
  assert.equal(await session.graph.readOutput('a', 'out'), 8);
});

/** One of a stop's variables, by name. */
function variable(stop: LineStop, name: string) {
  const found = stop.variables.find((candidate) => candidate.name === name);
  assert.ok(found, `no \`${name}\` in ${stop.variables.map((other) => other.name)}`);
  return found;
}

test('a stop says what its variables hold, and which are in scope', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('walk-first'));
  await session.graph.inject('a', 'in', 5);
  const tick = session.tick();

  const first = await nextStop();
  assert.equal(variable(first, 'x').value, '5');
  assert.equal(variable(first, 'x').type, 'u32');
  assert.ok(!first.variables.some(({ name }) => name === 'a'), 'not yet');

  lines.stepOver();
  const second = await nextStop();
  assert.equal(second.line, mark('walk-second'));
  assert.equal(variable(second, 'a').value, '6', 'its value, now it has one');

  lines.stepIn();
  const body = await nextStop();
  assert.equal(body.line, mark('add-body'), 'past its opening line, to where its parameters are');
  assert.match(body.frames[0].function, /add/);
  assert.equal(variable(body, 'a').value, '6', "the callee's own `a`");
  assert.equal(variable(body, 'b').value, '2');
  assert.ok(!body.variables.some(({ name }) => name === 'x'), "its caller's is out of scope");

  lines.clearBreakpoint(file, mark('walk-first'));
  lines.resume();
  await tick;
});

test('a variable of each kind reads as its source would write it', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('kinds-end'));
  await session.graph.inject('a', 'kinds', 4);
  const tick = session.tick();
  const stop = await nextStop();
  assert.equal(stop.line, mark('kinds-end'));

  const values = Object.fromEntries(stop.variables.map(({ name, value }) => [name, value]));
  assert.deepEqual(
    {
      seed: values.seed,
      count: values.count,
      negative: values.negative,
      ratio: values.ratio,
      flag: values.flag,
      letter: values.letter,
      point: values.point,
      text: values.text,
      owned: values.owned,
      list: values.list,
      slice: values.slice,
      array: values.array,
      some: values.some,
      none: values.none,
      dot: values.dot,
      circle: values.circle,
      rect: values.rect,
      pair: values.pair,
      reference: values.reference,
      boxed: values.boxed,
    },
    {
      seed: '4',
      count: '5',
      negative: '-11',
      ratio: '1.5',
      flag: 'true',
      letter: "'x'",
      point: 'Point { x: 3, y: 2.5 }',
      text: '"hello"',
      owned: '"owned"',
      list: '[10, 20, 30]',
      slice: '[10, 20, 30]',
      array: '[1, 2, 3]',
      some: 'Some(7)',
      none: 'None',
      dot: 'Dot',
      circle: 'Circle(0.5)',
      rect: 'Rect { w: 4, h: 5 }',
      pair: '(1, 2)',
      reference: '→ Point { x: 3, y: 2.5 }',
      boxed: '→ 9',
    },
  );

  // Their parts, to look into.
  assert.deepEqual(
    variable(stop, 'point').children.map(({ name, type, value }) => [name, type, value]),
    [
      ['x', 'i32', '3'],
      ['y', 'f64', '2.5'],
    ],
  );
  assert.deepEqual(
    variable(stop, 'list').children.map(({ name, value }) => [name, value]),
    [
      ['0', '10'],
      ['1', '20'],
      ['2', '30'],
    ],
  );
  assert.deepEqual(
    variable(stop, 'rect').children.map(({ name, value }) => [name, value]),
    [
      ['w', '4'],
      ['h', '5'],
    ],
  );
  assert.equal(variable(stop, 'reference').children[0].value, '3');

  lines.resume();
  await tick;
  await session.settle();
});

test('a frame says the line it is waiting on', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('add-body'));
  const tick = session.tick();
  const stop = await nextStop();
  assert.equal(stop.frames[0].function, 'line_walk_guest::add');
  assert.equal(stop.frames[0].line, mark('add-body'));
  assert.equal(stop.frames[0].file, file);
  assert.equal(stop.frames[1].function, 'line_walk_guest::walk');
  assert.equal(stop.frames[1].line, mark('walk-first'), 'the call it made');
  assert.equal(stop.frames[1].file, file);
  lines.release();
  await tick;
});

test('a host says which lines are its own', async () => {
  const guessed = await load([['a', 'line-walk']]);
  const directory = guessed.file.slice(0, -'lib.rs'.length);
  const under = await load([['a', 'line-walk']], [], {}, { own: [directory] });
  assert.deepEqual(under.lines.files, guessed.lines.files);
  const nowhere = await load([['a', 'line-walk']], [], {}, { own: ['/nowhere/'] });
  assert.deepEqual(nowhere.lines.files, []);
  await nowhere.session.settle();
  assert.equal(await nowhere.session.graph.readOutput('a', 'out'), 3);
});

test('a line with no code comes to the next that has it', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  const asked = mark('above-walk');
  const actual = lines.setBreakpoint(file, asked) as number;
  assert.ok(actual > asked);
  assert.ok(lines.lines(file).includes(actual));
  assert.ok(!lines.lines(file).includes(asked));
  assert.equal(lines.setBreakpoint(file, 100000), undefined, 'past the last line');
  const tick = session.tick();
  assert.equal((await nextStop()).line, actual);
  lines.resume();
  await tick;
});

test('stepping goes over a call, into one, and out of it', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('walk-first'));
  const tick = session.tick();
  const first = await nextStop();
  assert.equal(first.line, mark('walk-first'));

  lines.clearBreakpoint(file, mark('walk-first'));
  lines.stepOver();
  const second = await nextStop();
  assert.equal(second.reason, 'step');
  assert.equal(second.line, mark('walk-second'), 'over the call of `add`');
  assert.equal(second.frames.length, first.frames.length);

  lines.stepIn();
  const inside = await nextStop();
  assert.match(inside.frames[0].function, /add/);
  assert.ok(inside.frames.length > second.frames.length, 'a frame deeper');

  lines.stepOut();
  const back = await nextStop();
  assert.match(back.frames[0].function, /walk/);
  assert.equal(back.frames.length, second.frames.length);

  lines.resume();
  await tick;
});

test('pausing stops a guest in the middle of a spin', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  await session.graph.inject('a', 'spin', 4_000_000_000);
  const tick = session.tick();
  let ended = false;
  void tick.then(
    () => {
      ended = true;
    },
    () => {},
  );
  await new Promise((resolve) => setTimeout(resolve, 150));
  lines.pause();
  const stop = await nextStop();
  assert.equal(stop.reason, 'pause');
  assert.equal(stop.file, file);
  assert.equal(stop.node, 'a');
  assert.match(stop.frames[0].function, /spin/);
  assert.equal(ended, false);

  // A breakpoint set while it spins is seen too, with nothing in the worker reading messages.
  lines.resume();
  await new Promise((resolve) => setTimeout(resolve, 50));
  lines.setBreakpoint(file, mark('spin-body'));
  const hit = await nextStop();
  assert.equal(hit.reason, 'breakpoint');
  assert.equal(hit.line, mark('spin-body'));
});

test('a stop says whose guest it is, whichever order the graph has them in', async () => {
  for (const [first, second] of [
    ['x', 'y'],
    ['y', 'x'],
  ]) {
    for (const [held, free] of [
      [first, second],
      [second, first],
    ]) {
      const release: Record<string, () => void> = {};
      const { session, lines, nextStop, file } = await load(
        [
          [first, 'line-walk'],
          [second, 'line-walk'],
        ],
        [],
        {
          holds: [held],
          beforeRun: (node, isHeld) => {
            if (isHeld) {
              return new Promise((resolve) => {
                release[node] = resolve;
              });
            }
          },
        },
      );
      lines.setBreakpoint(file, mark('walk-first'));
      const tick = session.tick();
      const stop = await nextStop();
      assert.equal(stop.node, free, 'only the one not held is running');
      lines.resume();
      for (let at = 0; at < 400 && !release[held]; at++) {
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
      release[held]();
      const next = await nextStop();
      assert.equal(next.node, held);
      assert.notEqual(next.instance, stop.instance);
      lines.resume();
      await tick;
    }
  }
});

test('a stop names the right node when connections decide the order', async () => {
  for (const nodes of [
    ['a', 'b'],
    ['b', 'a'],
  ]) {
    // Whichever is declared first, `a` feeds `b`.
    const { session, lines, nextStop, file } = await load(
      nodes.map((node): [string, string] => [node, 'line-walk']),
      [['ab', 'a.out', 'b.in']],
    );
    lines.setBreakpoint(file, mark('walk-first'));
    await session.graph.inject('a', 'in', 5);
    const tick = session.tick();
    for (const [node, x] of [
      ['a', '5'],
      ['b', '8'],
    ]) {
      const stop = await nextStop();
      assert.equal(stop.node, node);
      assert.equal(variable(stop, 'x').value, x, node);
      lines.resume();
    }
    await tick;
  }
});

test('a mocked node has no instance to count', async () => {
  const { session, lines, nextStop, file } = await load(
    [
      ['mocked', 'line-walk'],
      ['real', 'line-walk'],
    ],
    [],
    { mocks: { mocked: () => ({ out: 1 }) } },
  );
  lines.setBreakpoint(file, mark('walk-first'));
  const tick = session.tick();
  assert.equal((await nextStop()).node, 'real');
  lines.resume();
  await tick;
});

test('a cleared breakpoint does not stop, and one set stays through a rebuild', async () => {
  const { session, lines, nextStop, file } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('walk-first'));
  lines.clearBreakpoint(file, mark('walk-first'));
  await session.settle();
  assert.equal(lines.stopped, undefined);
  assert.equal(await session.graph.readOutput('a', 'out'), 3);

  // A new instance, with the breakpoint already set.
  lines.setBreakpoint(file, mark('walk-second'));
  session.graph.shutdown();
  session.graph.rerun('a');
  const tick = session.tick();
  assert.equal((await nextStop()).line, mark('walk-second'));
  lines.resume();
  await tick;
});

test('a step stays with its guest, while the others run past their probes', async () => {
  const { session, lines, nextStop, file } = await load(
    [
      ['a', 'line-walk'],
      ['b', 'line-walk'],
    ],
    [['ab', 'a.out', 'b.in']],
  );
  lines.setBreakpoint(file, mark('walk-first'));
  const tick = session.tick();
  const first = await nextStop();
  assert.equal(first.node, 'a');
  lines.clearBreakpoint(file, mark('walk-first'));

  let result: TickResult | undefined;
  void tick.then((value) => {
    result = value;
  });
  for (let at = 0; at < 30; at++) {
    lines.stepOver();
    const stop = await Promise.race([
      nextStop(),
      new Promise<undefined>((resolve) => setTimeout(resolve, 400)),
    ]);
    if (!stop) break;
    assert.equal(stop.node, 'a', 'only its own guest');
    if (result) break;
  }
  lines.release();
  await tick;
  await session.settle();
  assert.equal(await session.graph.readOutput('b', 'out'), 6);
});

test('disposing a session stopped on a line lets the worker go on to the next', async () => {
  const worker = realWorker();
  const stopped = Promise.withResolvers<LineStop>();
  const first = await RemoteSession.load(
    worker,
    await options([['a', 'line-walk']], [], { lines: { onStop: stopped.resolve } }),
  );
  const firstLines = first.lines as LineDebugger;
  const file = firstLines.files.find((name) => name.endsWith('line-walk/src/lib.rs')) as string;
  firstLines.setBreakpoint(file, mark('walk-first'));
  void first.tick().catch(() => {});
  await stopped.promise;
  first.dispose();

  const second = await RemoteSession.load(worker, await options([['a', 'line-walk']]));
  assert.equal(second.lines, undefined, 'none unless asked');
  await second.graph.inject('a', 'in', 1);
  await second.settle();
  assert.equal(await second.graph.readOutput('a', 'out'), 4);
  second.dispose();
});

test('ending the worker ends a guest stopped on a line', async () => {
  const { session, lines, nextStop, file, worker } = await load([['a', 'line-walk']]);
  lines.setBreakpoint(file, mark('walk-first'));
  const tick = session.tick();
  await nextStop();
  assert.equal(endWorker(worker), true);
  await assert.rejects(tick, /ended/);
});

test('a page that is not isolated is told why it cannot debug lines', async () => {
  const worker = realWorker();
  const scope = globalThis as { crossOriginIsolated?: boolean };
  scope.crossOriginIsolated = false;
  try {
    await assert.rejects(
      RemoteSession.load(
        worker,
        await options([['a', 'line-walk']], [], { lines: { onStop() {} } }),
      ),
      /shared memory.*Cross-Origin-Opener-Policy/s,
    );
  } finally {
    delete scope.crossOriginIsolated;
  }
});

test('unasked for, a guest with debug info runs as it was built, with no probes', async () => {
  const seen: Uint8Array[] = [];
  const compile = WebAssembly.compile;
  WebAssembly.compile = (bytes: BufferSource) => {
    seen.push(new Uint8Array(ArrayBuffer.isView(bytes) ? bytes.buffer : bytes));
    return compile(bytes);
  };
  try {
    const { graph, components } = await options([['a', 'line-walk']]);
    const session = await Session.load({ graph, components, wasm });
    const named = (bytes: Uint8Array, text: string) => Buffer.from(bytes).includes(text);
    assert.ok(
      seen.some((bytes) => named(bytes, '.debug_line')),
      'it reached the engine as it was',
    );
    assert.ok(!seen.some((bytes) => named(bytes, 'witgraph:debug')), 'with nothing added');
    assert.deepEqual(session.graph.moduleNodes(), {});
    session.dispose();
  } finally {
    WebAssembly.compile = compile;
  }
});

test('a guest with no debug info has no lines, and runs', async () => {
  const { session, lines } = await load(
    [
      ['r', 'relay'],
      ['s', 'relay'],
    ],
    [
      ['loop', 'r.out', 'r.in', true],
      ['rs', 'r.out', 's.in'],
    ],
  );
  assert.deepEqual(lines.files, []);
  await session.graph.inject('r', 'in', 2);
  await session.settle();
  assert.equal(await session.graph.readOutput('s', 'out'), 2);
});

test("all lines reach into what is not the guest's own", async () => {
  const { lines, file } = await load([['a', 'line-walk']], [], {}, { all: true });
  assert.ok(file);
  assert.ok(lines.files.some((name) => name.includes('/rustlib/') || name.includes('/rustc/')));
  const own = await load([['a', 'line-walk']]);
  assert.deepEqual(own.lines.files, [own.file]);
});

test('without a debugger, a session calls what it was given at each set flag', async () => {
  const hits: [number, number, number][] = [];
  let told: { probes: number; nodes: string[] }[] = [];
  let instances = 0;
  const { graph, components } = await options([
    ['a', 'line-walk'],
    ['b', 'line-walk'],
  ]);
  const session = await Session.load({
    graph,
    components,
    wasm,
    lines: {
      modules(modules) {
        told = modules.map((module) => ({ probes: module.probes.length, nodes: module.nodes }));
      },
      instance(_module, _instance, buffer) {
        instances += 1;
        new Uint8Array(buffer).fill(1);
      },
      probe(module, instance, id) {
        hits.push([module, instance, id]);
      },
    },
  });
  assert.equal(told.length, 1, 'both nodes are instances of one module');
  assert.ok(told[0].probes > 0);
  assert.deepEqual(told[0].nodes, ['a', 'b']);
  assert.equal(instances, 2);
  await session.settle();
  assert.ok(hits.some(([, instance]) => instance === 0));
  assert.ok(hits.some(([, instance]) => instance === 1));
  session.dispose();
});

test('serving on the thread a session asks from can still load without lines', async () => {
  // A channel in one thread cannot stop, as nothing could let it go on, but runs all the same.
  const { port1, port2 } = new MessageChannel();
  serveSession(port2 as unknown as Endpoint);
  const session = await RemoteSession.load(
    port1 as unknown as Endpoint,
    await options([['a', 'line-walk']]),
  );
  await session.settle();
  assert.equal(await session.graph.readOutput('a', 'out'), 3);
  session.dispose();
  port1.close();
  port2.close();
});
