// Runs the runtime's own guest components under the JavaScript executor.
// Build them first: `cargo build -p test-components`, then
// `npm run build:wasm`.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFile, readdir, stat } from 'node:fs/promises';
import { afterEach, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { parse } from '@bytecodealliance/jco';

import {
  Session,
  WebGraph,
  camelCase,
  jsName,
  type Capability,
  type ComponentSource,
  type SessionOptions,
  type StreamItem,
} from '../src/index.ts';

const root = new URL('../../../', import.meta.url);

/** Cargo's target directory, wherever `CARGO_TARGET_DIR` or the config
 * puts it. */
function targetDir(): URL {
  const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: fileURLToPath(root),
    encoding: 'utf8',
  });
  return pathToFileURL(`${JSON.parse(metadata).target_directory}/`);
}

/** The directory cargo built the guest components into. */
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
const wasm = await readFile(new URL('../pkg/witgraph_web_bg.wasm', import.meta.url));

const ids: Record<string, string> = {
  echo: 'test:echo/echo@0.1.0',
  relay: 'test:relay/relay@0.1.0',
  'stream-producer': 'test:stream-producer/stream-producer@0.1.0',
  'stream-consumer': 'test:stream-consumer/stream-consumer@0.1.0',
  'busy-loop': 'test:busy-loop/busy-loop@0.1.0',
  'mqtt-node': 'test:mqtt-node/mqtt-node@0.1.0',
  labelled: 'test:labelled/labelled@0.1.0',
  'future-writer': 'test:future-writer/future-writer@0.1.0',
};

/** Components written in WAT here, by guest name, beside the built guests. */
const inline: Record<string, ComponentSource> = {};

/**
 * `values`: echoes nested options and a list, in the representation jco
 * picks for each (`option<option<u32>>` is tagged, the option around it
 * is nullable again).
 */
inline.values = {
  id: 'test:values/values@0.1.0',
  bytes: await parse(`(component
    (type $o1 (option u32))
    (type $o2 (option $o1))
    (type $o3 (option $o2))
    (type $xs (option (list u32)))
    (type $inputs (record (field "a" $o2) (field "b" $o3) (field "xs" $xs)))
    (type $outputs (record (field "a" $o2) (field "b" $o3) (field "xs" $xs)))
    (core module $mem (memory (export "memory") 4))
    (core instance $m (instantiate $mem))
    (alias core export $m "memory" (core memory $memory))
    (core func $tr (canon task.return (result $outputs) (memory $memory)))
    (core module $main
      (import "host" "task-return"
        (func $task_return (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)))
      (import "env" "memory" (memory 4))
      (global $next (mut i32) (i32.const 16))
      ;; A bump allocator: the few runs of a test fit.
      (func (export "realloc") (param i32 i32 i32 i32) (result i32)
        (local $at i32)
        (local.set $at (global.get $next))
        (global.set $next
          (i32.and (i32.add (i32.add (local.get $at) (local.get 3)) (i32.const 7)) (i32.const -8)))
        (local.get $at))
      ;; Returns its inputs at once: task.return, then EXIT (0).
      (func (export "run") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
        (call $task_return
          (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)
          (local.get 5) (local.get 6) (local.get 7) (local.get 8) (local.get 9))
        (i32.const 0))
      (func (export "callback") (param i32 i32 i32) (result i32) unreachable))
    (core instance $i (instantiate $main
      (with "host" (instance (export "task-return" (func $tr))))
      (with "env" (instance (export "memory" (memory $memory))))))
    (func $run async (param "inputs" $inputs) (result $outputs)
      (canon lift (core func $i "run") async (memory $memory) (realloc (core func $i "realloc"))
        (callback (core func $i "callback"))))
    (component $shim
      (type $o1 (option u32))
      (type $o2 (option $o1))
      (type $o3 (option $o2))
      (type $xs (option (list u32)))
      (type $in (record (field "a" $o2) (field "b" $o3) (field "xs" $xs)))
      (type $out (record (field "a" $o2) (field "b" $o3) (field "xs" $xs)))
      (import "import-type-inputs" (type $ti (eq $in)))
      (import "import-type-outputs" (type $to (eq $out)))
      (import "import-func-run" (func $f async (param "inputs" $ti) (result $to)))
      (export $ei "inputs" (type $ti))
      (export $eo "outputs" (type $to))
      (export "run" (func $f) (func async (param "inputs" $ei) (result $eo))))
    (instance $node (instantiate $shim
      (with "import-type-inputs" (type $inputs))
      (with "import-type-outputs" (type $outputs))
      (with "import-func-run" (func $run))))
    (export "node" (instance $node)))`),
  wit: `package test:values@0.1.0;
    world values {
      export node: interface {
        record inputs {
          a: option<option<u32>>,
          b: option<option<option<u32>>>,
          xs: option<list<u32>>,
        }
        record outputs {
          a: option<option<u32>>,
          b: option<option<option<u32>>>,
          xs: option<list<u32>>,
        }
        run: async func(inputs: inputs) -> outputs;
      }
    }`,
};

/**
 * `start-fatal`: its start function calls `fatal` when the host's
 * `should-fail` says so; its `run` returns `out: 7`. The wasmtime
 * runtime's own test component.
 */
inline['start-fatal'] = {
  id: 'demo:startfatal/w@0.1.0',
  bytes: await parse(`(component
    (import "demo:startfatal/ctl@0.1.0" (instance $ctl
      (export "should-fail" (func (result bool)))))
    (import "witgraph:runtime/host@0.1.0" (instance $host
      (export "fatal" (func (param "message" string)))))
    (core module $mem (memory (export "memory") 1))
    (core instance $m (instantiate $mem))
    (alias core export $m "memory" (core memory $memory))
    (alias export $ctl "should-fail" (func $should_fail))
    (alias export $host "fatal" (func $fatal))
    (core func $sf (canon lower (func $should_fail)))
    (core func $fl (canon lower (func $fatal) (memory $memory)))
    (type $outputs (record (field "out" u32)))
    (core func $tr (canon task.return (result $outputs)))
    (core module $main
      (import "ctl" "should-fail" (func $should_fail (result i32)))
      (import "host" "fatal" (func $fatal (param i32 i32)))
      (import "host" "task-return" (func $task_return (param i32)))
      (import "env" "memory" (memory 1))
      (data (i32.const 16) "boom")
      (func $start
        (if (call $should_fail)
          (then (call $fatal (i32.const 16) (i32.const 4)))))
      (start $start)
      (func (export "run") (result i32)
        (call $task_return (i32.const 7))
        (i32.const 0))
      (func (export "callback") (param i32 i32 i32) (result i32) unreachable))
    (core instance $i (instantiate $main
      (with "ctl" (instance (export "should-fail" (func $sf))))
      (with "host" (instance
        (export "fatal" (func $fl))
        (export "task-return" (func $tr))))
      (with "env" (instance (export "memory" (memory $memory))))))
    (func $run async (result $outputs)
      (canon lift (core func $i "run") async (callback (core func $i "callback"))))
    (component $shim
      (type $rec (record (field "out" u32)))
      (import "import-type-rec" (type $r (eq $rec)))
      (import "import-func-run" (func $f async (result $r)))
      (export $ro "outputs" (type $r))
      (export "run" (func $f) (func async (result $ro))))
    (instance $node (instantiate $shim
      (with "import-type-rec" (type $outputs))
      (with "import-func-run" (func $run))))
    (export "node" (instance $node)))`),
  wit: `package demo:startfatal@0.1.0;
    interface ctl { should-fail: func() -> bool; }
    world w {
      import ctl;
      import witgraph:runtime/host@0.1.0;
      export node: interface {
        record outputs { out: u32 }
        run: async func() -> outputs;
      }
    }
    package witgraph:runtime@0.1.0 {
      interface host { fatal: func(message: string); }
    }`,
};

function componentRef(id: string) {
  const match = /^([^:]+):([^/]+)\/([^@]+)@(.+)$/.exec(id)!;
  return { package: { namespace: match[1], name: match[2], version: match[4] }, world: match[3] };
}

type Connection = [id: string, from: string, to: string, feedback?: boolean];

/** Every session a test made, disposed after it. */
const sessions: Session[] = [];
afterEach(() => {
  for (const session of sessions.splice(0)) session.dispose();
});

/** `Session.load`, with the session disposed after the test. */
async function loaded(options: SessionOptions): Promise<Session> {
  const session = await Session.load(options);
  sessions.push(session);
  return session;
}

/** A built guest, or one written in WAT here. */
async function guestSource(guest: string): Promise<ComponentSource> {
  return (
    inline[guest] ?? {
      id: ids[guest],
      bytes: new Uint8Array(await readFile(new URL(`${guest}.wasm`, guests))),
    }
  );
}

/** A graph over the named guests: nodes are `[id, guest]`, ports are `node.port`. */
async function load(
  nodes: [string, string][],
  connections: Connection[] = [],
  options: Partial<SessionOptions> = {},
): Promise<Session> {
  const used = [...new Set(nodes.map(([, guest]) => guest))];
  const components = await Promise.all(used.map(guestSource));
  const port = (text: string) => {
    const [node, name] = text.split('.');
    return { node, port: name };
  };
  const graph = {
    metadata: { name: 'test' },
    components: components.map(({ id }) => componentRef(id)),
    nodes: nodes.map(([id, guest]) => ({ id, component: componentRef(inline[guest]?.id ?? ids[guest]) })),
    connections: connections.map(([id, from, to, feedback]) => ({
      id,
      from: port(from),
      to: port(to),
      ...(feedback ? { feedback: true } : {}),
    })),
  };
  return loaded({ graph, components, wasm, ...options });
}

test('values flow between nodes and equal inputs do not rerun', async () => {
  const session = await load(
    [['a', 'echo'], ['b', 'echo']],
    [['c', 'a.out', 'b.in']],
  );
  session.graph.inject('a', 'in', 5);
  await session.settle();
  assert.equal(session.graph.readOutput('a', 'out'), 5);
  assert.equal(session.graph.readOutput('b', 'out'), 5);
  assert.equal(session.graph.readOutputWave('b', 'out'), '5');

  const runs = () => session.graph.takeTrace().filter((e: any) => e.event === 'run-started');
  assert.equal(runs().length, 2);
  session.graph.inject('a', 'in', 5);
  await session.settle();
  assert.equal(runs().length, 0, 'the same value is not a change');
  session.graph.injectWave('a', 'in', '6.5');
  await session.settle();
  assert.deepEqual(runs().map((e: any) => e.node), ['a', 'b']);
  assert.equal(session.graph.readOutput('b', 'out'), 6.5);

  assert.throws(() => session.graph.inject('a', 'in', 'five'), /expected a number/);
  assert.throws(() => session.graph.inject('b', 'in', 1), /written by connection/);
  // The connection is the error, whatever the value.
  assert.throws(() => session.graph.inject('b', 'in', 'five'), /written by connection/);
  assert.throws(() => session.graph.inject('a', 'nope', 1), /not a Value input port/);
  assert.throws(() => session.graph.readOutput('x', 'out'), /unknown node `x`/);
  assert.throws(() => session.graph.readOutput('a', 'nope'), /not a Value output port/);
});

test('a stream passes between two nodes of an island, and a tap sees its items', async () => {
  const seen: StreamItem[] = [];
  const session = await load(
    [['prod', 'stream-producer'], ['cons', 'stream-consumer']],
    [['s', 'prod.items', 'cons.items']],
    { onStreamItem: (item) => seen.push(item) },
  );
  assert.deepEqual(session.graph.islands(), [['prod', 'cons']]);
  session.graph.inject('prod', 'burst-size', 5);
  await session.settle();
  assert.equal(session.graph.readOutput('cons', 'count'), 5);
  assert.equal(session.graph.readOutput('cons', 'total'), 10);
  assert.equal(session.graph.readOutput('prod', 'limit'), 5);
  const items = seen.flatMap(({ node, port, item }) => {
    assert.deepEqual([node, port], ['prod', 'items']);
    return Array.from(item as Uint32Array);
  });
  assert.deepEqual(items, [0, 1, 2, 3, 4]);
});

test('a feedback loop advances one iteration per tick, and a snapshot carries it on', async () => {
  const nodes: [string, string][] = [['n', 'relay']];
  const loop: Connection[] = [['loop', 'n.out', 'n.in', true]];
  const session = await load(nodes, loop);
  session.graph.inject('n', 'in', 1);
  session.graph.inject('n', 'add', 1);
  for (const expected of [2, 3, 4]) {
    assert.deepEqual(await session.tick(), { result: 'progress' });
    assert.equal(session.graph.readOutput('n', 'out'), expected);
  }
  const snapshot = session.graph.snapshot();
  assert.equal(JSON.parse(snapshot).outputs.n.out, '4');

  const restored = await load(nodes, loop);
  restored.graph.restore(snapshot);
  assert.equal(restored.graph.nodeState('n').phase, 'pending');
  await restored.tick();
  assert.equal(restored.graph.readOutput('n', 'out'), 5);
});

test('a mock replaces a node, and the hooks see every run', async () => {
  const calls: string[] = [];
  const session = await load(
    [['a', 'echo'], ['b', 'echo']],
    [['c', 'a.out', 'b.in']],
    {
      mocks: { a: (inputs) => ({ out: (inputs.in as number) * 100 }) },
      beforeRun: (node, inputs) => void calls.push(`before ${node} ${JSON.stringify(inputs)}`),
      afterRun: (node, outputs) => void calls.push(`after ${node} ${JSON.stringify(outputs)}`),
    },
  );
  session.graph.inject('a', 'in', 2);
  await session.settle();
  assert.equal(session.graph.readOutput('b', 'out'), 200);
  assert.deepEqual(calls, [
    'before a {"in":2}',
    'after a {"out":200}',
    'before b {"in":200}',
    'after b {"out":200}',
  ]);
});

test('a trap faults its island, and fatal aborts the tick', async () => {
  const session = await load([['busy', 'busy-loop'], ['e', 'echo']]);
  session.graph.inject('e', 'in', 1);
  session.graph.inject('busy', 'action', 'trap');
  assert.deepEqual(await session.tick(), { result: 'progress' });
  const [fault] = session.graph.takeFaults();
  assert.deepEqual(fault.members, ['busy']);
  assert.equal(fault.fault.kind, 'trap');
  assert.equal(session.graph.nodeState('busy').phase, 'faulted');
  assert.equal(session.graph.readOutput('e', 'out'), 1, 'the other island ran');

  session.graph.inject('busy', 'action', 'fatal');
  const tick = await session.tick();
  assert.equal(tick.result, 'aborted');
  assert.equal((tick as any).node, 'busy');
  assert.equal(session.graph.nodeState('busy').fault.kind, 'fatal');

  // A new input rebuilds the island.
  session.graph.inject('busy', 'action', 'finish');
  await session.settle();
  assert.equal(session.graph.nodeState('busy').phase, 'idle');
  assert.ok(session.graph.takeTrace().some((e: any) => e.event === 'restarted'));
});

test('capabilities and labelled imports come from the registry, named as on wasmtime', async () => {
  const feed = [3, 4, 5];
  const asked: [string, Capability][] = [];
  const session = await load(
    [['mqtt', 'mqtt-node'], ['cons', 'stream-consumer'], ['kv', 'labelled'], ['busy', 'busy-loop']],
    [['s', 'mqtt.messages', 'cons.items']],
    {
      capabilities: (node, capability) => {
        asked.push([node, capability]);
        if (capability.interface === 'test:mqtt/source@0.1.0') {
          return { nextMessage: () => feed.shift() };
        }
        if (capability.implements === 'test:kv/store@0.1.0') {
          return { get: (key: string) => `${capability.interface}:${key}@${node}` };
        }
      },
    },
  );
  session.graph.inject('kv', 'key', 'k');
  session.graph.inject('busy', 'action', 'finish');
  await session.settle();
  assert.equal(session.graph.readOutput('cons', 'total'), 12);
  assert.equal(session.graph.readOutput('kv', 'primary'), 'primary:k@kv');
  assert.equal(session.graph.readOutput('kv', 'backup'), 'backup:k@kv');
  // The contract's capabilities, version and labelled interface included;
  // the built-in host is never asked for.
  const seen = asked.map(([node, { interface: name, implements: of }]) => [node, name, of]);
  assert.deepEqual(seen.sort(), [
    ['kv', 'backup', 'test:kv/store@0.1.0'],
    ['kv', 'primary', 'test:kv/store@0.1.0'],
    ['mqtt', 'test:mqtt/source@0.1.0', undefined],
  ]);
  assert.ok(asked.every(([, capability]) => capability.items !== undefined));
});

test('a graph that does not compile reports why', async () => {
  await assert.rejects(load([['a', 'relay']]), /required input port `a.in` is unconnected/);
});

test('values compare as on wasmtime: NaN equals NaN, -0 differs from 0', async () => {
  const session = await load([['n', 'echo']], [['loop', 'n.out', 'n.in', true]]);
  session.graph.inject('n', 'in', Number.NaN);
  await session.settle();
  assert.ok(Number.isNaN(session.graph.readOutput('n', 'out')), 'a NaN loop settles');

  const runs = () => session.graph.takeTrace().filter((e: any) => e.event === 'run-started');
  session.graph.inject('n', 'in', 0);
  await session.settle();
  runs();
  session.graph.inject('n', 'in', -0);
  await session.settle();
  assert.equal(runs().length, 1, '-0 is a change');
});

test('a paused tick can be inspected', async () => {
  let resume!: () => void;
  let paused!: () => void;
  const pause = new Promise<void>((resolve) => (paused = resolve));
  const session = await load([['a', 'echo'], ['b', 'echo']], [['c', 'a.out', 'b.in']], {
    beforeRun: (node) =>
      node === 'b' ? new Promise<void>((resolve) => ((resume = resolve), paused())) : undefined,
  });
  session.graph.inject('a', 'in', 7);
  const tick = session.tick();
  await pause;
  assert.equal(session.graph.readOutput('a', 'out'), 7, 'what `a` returned');
  assert.equal(session.graph.readOutputWave('a', 'out'), '7');
  assert.equal(session.graph.readOutput('b', 'out'), undefined, '`b` has not returned');
  assert.equal(session.graph.nodeState('b').phase, 'running');
  assert.ok(session.graph.takeTrace().some((e: any) => e.event === 'run-started' && e.node === 'b'));
  assert.throws(() => session.graph.inject('a', 'in', 1), /a tick is in progress/);
  resume();
  await tick;
  assert.equal(session.graph.readOutput('b', 'out'), 7);
});

test('with its WIT source, a component resolves against the source contract', async () => {
  const wit = await readFile(
    new URL('test-fixtures/test-components/guests/echo/wit/world.wit', root),
    'utf8',
  );
  const bytes = new Uint8Array(await readFile(new URL('echo.wasm', guests)));
  const plain = await load([['a', 'echo']]);
  const hash = plain.contracts.get('a')!.id.split('#')[1];
  const pinned = { ...componentRef(ids.echo), content_hash: hash };
  const graph = {
    metadata: { name: 'pinned' },
    components: [pinned],
    nodes: [{ id: 'a', component: pinned }],
    connections: [],
  };
  // The component passed under the pinned id the graph's table uses.
  const session = await loaded({
    graph,
    components: [{ id: `${ids.echo}#${hash}`, bytes, wit }],
    wasm,
  });
  session.graph.inject('a', 'in', 3);
  await session.settle();
  assert.equal(session.graph.readOutput('a', 'out'), 3);

  const relay = await readFile(
    new URL('test-fixtures/test-components/guests/relay/wit/world.wit', root),
    'utf8',
  );
  await assert.rejects(
    Session.load({ graph, components: [{ id: ids.echo, bytes, wit: relay }], wasm }),
    /no component world/,
  );
});

test('names are camel-cased as jco does, acronyms included', async () => {
  const session = await load([['a', 'echo']]);
  assert.equal(camelCase('burst-size'), 'burstSize');
  assert.equal(camelCase('HTTP-status'), 'httpStatus');
  assert.equal(camelCase('get-HTTP'), 'getHttp');
  for (const name of ['burst-size', 'HTTP-status', 'get-HTTP', 'x', 'a-b-c', 'raw-RGB-2']) {
    assert.equal(camelCase(name), jsName(name), name);
  }
  // The same module may be passed again, as fresh bytes.
  const again = await loaded({
    graph: { metadata: { name: 'again' }, components: [], nodes: [], connections: [] },
    components: [],
    wasm: new Uint8Array(wasm),
  });
  assert.ok(again);
  // A disposed session's graph is gone.
  sessions.splice(sessions.indexOf(session), 1);
  session.dispose();
  assert.throws(() => session.graph.islands());
});

test('a run whose generation is abandoned stops', async () => {
  let resume!: () => void;
  let paused!: () => void;
  const pause = new Promise<void>((resolve) => (paused = resolve));
  const after: string[] = [];
  const session = await load([['a', 'echo']], [], {
    beforeRun: () => new Promise<void>((resolve) => ((resume = resolve), paused())),
    afterRun: (node) => void after.push(node),
  });
  session.graph.inject('a', 'in', 1);
  // Interrupt the tick while `a` is held, then cancel `a`: its generation
  // is dropped.
  let stop!: () => void;
  const tick = session.tick(new Promise<void>((resolve) => (stop = resolve)));
  await pause;
  stop();
  assert.equal((await tick).result, 'interrupted');
  session.graph.cancel('a');
  resume();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.deepEqual(after, [], 'the abandoned run did not go on');
});

/** Settles within `ms`, or fails: what a deadlock looks like here. */
async function within<T>(promise: Promise<T>, ms = 2000): Promise<T> {
  let timer!: ReturnType<typeof setTimeout>;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`did not settle in ${ms} ms`)), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

/** A promise, and what settles it. */
function gate(): { promise: Promise<void>; open: () => void } {
  let open!: () => void;
  const promise = new Promise<void>((resolve) => (open = resolve));
  return { promise, open };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 10));

test('each node runs the component its contract was resolved from', async () => {
  const echo = await guestSource('echo');
  const maybe = await guestSource('maybe');
  const plain = await load([['a', 'echo']]);
  const hash = plain.contracts.get('a')!.id.split('#')[1];
  const unpinned = componentRef(ids.echo);
  const pinned = { ...unpinned, content_hash: hash };
  const graph = {
    metadata: { name: 'pinned' },
    components: [pinned],
    nodes: [{ id: 'n', component: unpinned }],
    connections: [],
  };
  // Other bytes under the unpinned id: the table pins the echo's.
  for (const components of [
    [{ id: ids.echo, bytes: maybe.bytes }, { id: `${ids.echo}#${hash}`, bytes: echo.bytes }],
    [{ id: `${ids.echo}#${hash}`, bytes: echo.bytes }, { id: ids.echo, bytes: maybe.bytes }],
  ]) {
    const session = await loaded({ graph, components, wasm });
    assert.deepEqual(session.graph.component('n').index, components.findIndex((c) => c.bytes === echo.bytes));
    session.graph.inject('n', 'in', 4);
    await session.settle();
    assert.deepEqual(session.graph.takeFaults(), []);
    assert.equal(session.graph.readOutput('n', 'out'), 4);
  }
});

test('nested options cross in the representation jco picks at each depth', async () => {
  const runs: any[] = [];
  const session = await load([['n', 'values']], [], {
    afterRun: (_, outputs) => void runs.push(outputs),
  });
  const wave = (port: string) => session.graph.readOutputWave('n', port);
  // Absent optional inputs: `none` of `option<option<u32>>` is tagged.
  session.graph.inject('n', 'xs', []);
  await session.settle();
  assert.deepEqual(session.graph.takeFaults(), []);
  assert.deepEqual([wave('a'), wave('b')], ['none', 'none']);
  assert.deepEqual(session.graph.readOutput('n', 'a'), { tag: 'none' });
  assert.equal(session.graph.readOutput('n', 'b'), undefined);

  const cases: [a: string, b: string, jsA: unknown, jsB: unknown][] = [
    ['some(none)', 'some(none)', { tag: 'some', val: undefined }, { tag: 'none' }],
    ['some(some(5))', 'some(some(none))', { tag: 'some', val: 5 }, { tag: 'some', val: undefined }],
    ['none', 'some(some(some(7)))', { tag: 'none' }, { tag: 'some', val: 7 }],
  ];
  for (const [a, b, jsA, jsB] of cases) {
    // An optional input takes the payload: `a`'s is `option<u32>`.
    const payload = (text: string) => text.replace(/^some\((.*)\)$/, '$1');
    if (a === 'none') session.graph.clearInput('n', 'a');
    else session.graph.injectWave('n', 'a', payload(a));
    session.graph.injectWave('n', 'b', payload(b));
    await session.settle();
    assert.deepEqual(session.graph.takeFaults(), []);
    assert.deepEqual([wave('a'), wave('b')], [a, b], 'through jco and back');
    assert.deepEqual(session.graph.readOutput('n', 'a'), jsA);
    assert.deepEqual(session.graph.readOutput('n', 'b'), jsB);
    assert.deepEqual(runs.at(-1).a, jsA, 'as jco lifted it');
    assert.deepEqual(runs.at(-1).b, jsB, 'as jco lifted it');
  }
  // And in: `b`'s payload is `option<option<u32>>`, tagged.
  session.graph.inject('n', 'b', { tag: 'some', val: undefined });
  await session.settle();
  assert.equal(wave('b'), 'some(some(none))');
  session.graph.inject('n', 'b', { tag: 'none' });
  await session.settle();
  assert.equal(wave('b'), 'some(none)');
  assert.throws(() => session.graph.inject('n', 'b', 3), /expected `\{ tag: 'none' \| 'some', val \}`/);
});

test('lists cross as typed arrays and arrays', async () => {
  const session = await load([['n', 'values']]);
  session.graph.inject('n', 'xs', Uint32Array.of(1, 2, 3));
  await session.settle();
  const xs = session.graph.readOutput('n', 'xs');
  assert.ok(xs instanceof Uint32Array);
  assert.deepEqual(Array.from(xs), [1, 2, 3]);
  session.graph.inject('n', 'xs', [4, 5]);
  await session.settle();
  assert.equal(session.graph.readOutputWave('n', 'xs'), 'some([4, 5])');
  assert.throws(() => session.graph.inject('n', 'xs', [1, -1]), /item 1/);
  assert.throws(() => session.graph.inject('n', 'xs', Int8Array.of(-1)), /item 0/);
});

test('a value JavaScript fails to convert is an error, not a wedged graph', async () => {
  const { proxy, revoke } = Proxy.revocable({}, {});
  revoke();
  const session = await load([['n', 'values'], ['m', 'values']], [], {
    mocks: { m: () => ({ a: { tag: 'none' }, b: undefined, xs: { length: Infinity } }) },
  });
  for (const value of [proxy, { length: Infinity }]) {
    assert.throws(() => session.graph.inject('n', 'xs', value));
    assert.throws(() => session.graph.inject('n', 'b', value));
  }
  session.graph.inject('n', 'xs', [1]);
  session.graph.inject('m', 'xs', [1]);
  assert.deepEqual(await within(session.tick()), { result: 'progress' });
  const [fault] = session.graph.takeFaults();
  assert.deepEqual(fault.members, ['m']);
  assert.match(fault.fault.message, /returned a bad `xs`/);
  assert.equal(session.graph.readOutputWave('n', 'xs'), 'some([1])');
});

test('a tick started while one runs rejects at once', async () => {
  const nested: Promise<unknown>[] = [];
  const session = await load([['a', 'echo']], [], {
    beforeRun: async () => {
      nested.push(session.tick(), session.settle());
      await Promise.allSettled(nested);
    },
  });
  session.graph.inject('a', 'in', 1);
  assert.deepEqual(await within(session.tick()), { result: 'progress' });
  for (const result of await Promise.allSettled(nested)) {
    assert.equal(result.status, 'rejected');
    assert.match(String((result as PromiseRejectedResult).reason), /a tick is in progress/);
  }
  await within(session.settle());
});

test('a stop that is not a promise is refused, and the graph ticks on', async () => {
  const session = await load([['a', 'echo']]);
  session.graph.inject('a', 'in', 1);
  for (const stop of [new AbortController().signal, 5, {}]) {
    await assert.rejects(session.graph.tick(stop), /`stop` is not a promise/);
  }
  assert.deepEqual(await within(session.tick()), { result: 'progress' });
  assert.equal(session.graph.readOutput('a', 'out'), 1);
});

test('a failed run abandons its siblings still in flight', async () => {
  // Two producers whose consumers read each other's: one island, two roots.
  const held = gate();
  const paused = gate();
  const after: string[] = [];
  let holding = true;
  const session = await load(
    [['p1', 'stream-producer'], ['c1', 'stream-consumer'], ['m', 'stream-producer'], ['c2', 'stream-consumer']],
    [
      ['s1', 'p1.items', 'c1.items'],
      ['s2', 'm.items', 'c2.items'],
      ['t1', 'm.limit', 'c1.take'],
      ['t2', 'c1.total', 'c2.take'],
    ],
    {
      mocks: {
        p1: () => {
          throw new Error('p1 failed');
        },
      },
      beforeRun: async (node) => {
        if (node === 'm' && holding) {
          holding = false;
          paused.open();
          await held.promise;
        }
      },
      afterRun: (node) => void after.push(node),
    },
  );
  assert.deepEqual(session.graph.islands(), [['p1', 'm', 'c1', 'c2']]);
  session.graph.inject('m', 'burst-size', 2);
  const ticked = session.tick();
  await paused.promise;
  assert.deepEqual(await within(ticked), { result: 'progress' });
  const [fault] = session.graph.takeFaults();
  assert.equal(fault.culprit, 'p1');
  held.open();
  await tick();
  assert.deepEqual(after, [], 'the held run of `m` did not go on');
});

test('a disposed session stops its tick, and disposing twice is harmless', async () => {
  const held = gate();
  const paused = gate();
  const after: string[] = [];
  const session = await load([['a', 'echo']], [], {
    beforeRun: async () => {
      paused.open();
      await held.promise;
    },
    afterRun: (node) => void after.push(node),
  });
  sessions.splice(sessions.indexOf(session), 1);
  session.graph.inject('a', 'in', 1);
  const ticked = session.tick();
  await paused.promise;
  session.dispose();
  assert.deepEqual(await within(ticked), { result: 'interrupted' });
  held.open();
  await tick();
  assert.deepEqual(after, [], 'no hook runs for a disposed session');
  session.dispose();
  await assert.rejects(session.tick(), /disposed/);
});

test('a fatal start function during a rebuild aborts the tick, naming its node', async () => {
  let fail = false;
  const session = await load([['n', 'start-fatal']], [], {
    capabilities: (_, capability) =>
      capability.interface === 'demo:startfatal/ctl@0.1.0' ? { shouldFail: () => fail } : undefined,
  });
  assert.deepEqual(await session.tick(), { result: 'progress' });
  assert.equal(session.graph.readOutput('n', 'out'), 7);
  fail = true;
  session.graph.cancel('n');
  session.graph.rerun('n');
  const tick = await session.tick();
  assert.deepEqual(tick, { result: 'aborted', node: 'n', message: 'fatal: boom' });
  assert.equal(session.graph.nodeState('n').fault.kind, 'fatal');
  assert.equal(session.graph.nodeState('n').culprit, 'n');
});

test('a failed rebuild names the member whose rebuild failed', async () => {
  let provide = true;
  const session = await load(
    [['mqtt', 'mqtt-node'], ['cons', 'stream-consumer']],
    [['s', 'mqtt.messages', 'cons.items']],
    {
      capabilities: (_, capability) =>
        provide && capability.interface === 'test:mqtt/source@0.1.0'
          ? { nextMessage: () => undefined }
          : undefined,
    },
  );
  await session.settle();
  provide = false;
  session.graph.cancel('cons');
  session.graph.rerun('cons');
  assert.deepEqual(await session.tick(), { result: 'progress' });
  const [fault] = session.graph.takeFaults();
  assert.deepEqual(fault.members, ['mqtt', 'cons']);
  assert.equal(fault.culprit, 'mqtt');
  assert.equal(fault.fault.kind, 'restart');
  assert.match(fault.fault.message, /`mqtt`: node `mqtt` imports `test:mqtt\/source@0.1.0`, which nothing provides/);
});

test('a generation dropped while it rebuilds is abandoned', async () => {
  const { bytes } = await guestSource('echo');
  const graph = {
    metadata: { name: 'rebuild' },
    components: [componentRef(ids.echo)],
    nodes: [{ id: 'a', component: componentRef(ids.echo) }],
    connections: [],
  };
  const rebuilding = gate();
  const abandoned: string[][] = [];
  const web = WebGraph.load(JSON.stringify(graph), [{ id: ids.echo, bytes }], {
    run: () => ({ out: 1 }),
    rebuild: () => (rebuilding.open(), new Promise(() => {})),
    abandon: (nodes: string[]) => void abandoned.push(nodes),
  });
  try {
    web.inject('a', 'in', 1);
    await web.tick();
    web.cancel('a');
    web.rerun('a');
    let stop!: () => void;
    const ticked = web.tick(new Promise<void>((resolve) => (stop = resolve)));
    await rebuilding.promise;
    stop();
    assert.equal((await ticked).result, 'interrupted');
    assert.deepEqual(abandoned, []);
    web.cancel('a');
    assert.deepEqual(abandoned, [['a']]);
  } finally {
    web.free();
  }
  assert.throws(
    () => WebGraph.load(JSON.stringify(graph), [{ id: ids.echo, bytes }], { run: () => ({}) }),
    /`callbacks.rebuild` is required/,
  );
});

test('mocks are own properties, called as methods, with an empty result as no outputs', async () => {
  class Mocks {
    factor = 10;
    constructor() {
      Object.assign(this, { scaled: function (this: Mocks, inputs: any) { return { out: inputs.in * this.factor }; } });
    }
  }
  const after: [string, string[]][] = [];
  const session = await load(
    [['constructor', 'echo'], ['toString', 'echo'], ['scaled', 'echo'], ['none', 'echo']],
    [],
    {
      mocks: Object.assign(new Mocks() as any, { none: () => undefined }),
      afterRun: (node, outputs) => void after.push([node, Object.keys(outputs)]),
    },
  );
  for (const node of ['constructor', 'toString', 'scaled', 'none']) session.graph.inject(node, 'in', 2);
  await session.settle();
  assert.deepEqual(session.graph.takeFaults().map((f: any) => f.members), [['none']], 'only the empty mock is short of `out`');
  assert.equal(session.graph.readOutput('constructor', 'out'), 2, 'not mocked: it ran');
  assert.equal(session.graph.readOutput('toString', 'out'), 2, 'not mocked: it ran');
  assert.equal(session.graph.readOutput('scaled', 'out'), 20);
  assert.deepEqual(after.find(([node]) => node === 'none'), ['none', []]);
});

test('hooks are called as methods, and a throwing stream observer does not reach the reader', async () => {
  const sources = await Promise.all(['stream-producer', 'stream-consumer'].map(guestSource));
  const [prod, cons] = sources.map(({ id }) => componentRef(id));
  class Debugger implements SessionOptions {
    graph = {
      metadata: { name: 'debug' },
      components: [prod, cons],
      nodes: [
        { id: 'prod', component: prod },
        { id: 'cons', component: cons },
      ],
      connections: [{ id: 's', from: { node: 'prod', port: 'items' }, to: { node: 'cons', port: 'items' } }],
    };
    components = sources;
    wasm = wasm;
    before: string[] = [];
    items: unknown[] = [];
    beforeRun(node: string) {
      this.before.push(node);
    }
    onStreamItem({ item }: StreamItem) {
      this.items.push(item);
      throw new Error('observer failed');
    }
  }
  const debug = new Debugger();
  const errors: unknown[][] = [];
  const consoleError = console.error;
  console.error = (...args: unknown[]) => void errors.push(args);
  try {
    const session = await loaded(debug);
    session.graph.inject('prod', 'burst-size', 5);
    await session.settle();
    assert.deepEqual(session.graph.takeFaults(), []);
    assert.equal(session.graph.readOutput('cons', 'count'), 5);
    assert.deepEqual(debug.before, ['prod', 'cons']);
    assert.ok(debug.items.length > 0);
    assert.ok(errors.length > 0, 'what the observer threw is reported');
  } finally {
    console.error = consoleError;
  }
});

test('a future output nothing reads is consumed, so its writer finishes', async () => {
  const session = await load([['w', 'future-writer']]);
  await session.settle();
  // A generation here ends when `run` returns, before the writer's task
  // does: run again until a run reports an ended write.
  for (let again = 1; again <= 50 && session.graph.readOutput('w', 'last') === 0; again++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    session.graph.inject('w', 'again', again);
    await session.settle();
  }
  // Unlike wasmtime, which drops the read end (2), the loader takes the
  // value.
  assert.equal(session.graph.readOutput('w', 'last'), 1);
});

test('nothing needs a secure context', async () => {
  const subtle = Object.getOwnPropertyDescriptor(globalThis.crypto, 'subtle');
  Object.defineProperty(globalThis.crypto, 'subtle', { value: undefined, configurable: true });
  try {
    const session = await load([['a', 'echo']]);
    session.graph.inject('a', 'in', 1);
    await session.settle();
    assert.equal(session.graph.readOutput('a', 'out'), 1);
  } finally {
    if (subtle) Object.defineProperty(globalThis.crypto, 'subtle', subtle);
    else delete (globalThis.crypto as any).subtle;
  }
});
