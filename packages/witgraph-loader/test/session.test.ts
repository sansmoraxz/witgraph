// Runs the runtime's own guest components under the JavaScript executor.
// Build them first: `cargo build -p test-components`, then
// `npm run build:wasm`.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { afterEach, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { parse } from '@bytecodealliance/jco';
import { transpile } from '@bytecodealliance/jco/component';

import {
  type Capability,
  type ComponentSource,
  camelCase,
  type FutureItem,
  fastListLift,
  jsName,
  Session,
  type SessionOptions,
  type StreamItem,
  WebGraph,
  watchTraps,
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
  'bytes-consumer': 'test:bytes-consumer/bytes-consumer@0.1.0',
  'busy-loop': 'test:busy-loop/busy-loop@0.1.0',
  'mqtt-node': 'test:mqtt-node/mqtt-node@0.1.0',
  labelled: 'test:labelled/labelled@0.1.0',
  'future-writer': 'test:future-writer/future-writer@0.1.0',
  'future-reader': 'test:future-reader/future-reader@0.1.0',
  calc: 'test:calc/calc@0.1.0',
  math: 'test:math/math@0.1.0',
  'video-source': 'test:video-source/video-source@0.1.0',
  'frame-grayscale': 'test:frame-grayscale/frame-grayscale@0.1.0',
  'video-sink': 'test:video-sink/video-sink@0.1.0',
  'blob-source': 'test:blob-source/blob-source@0.1.0',
  'blob-relay': 'test:blob-relay/blob-relay@0.1.0',
  'blob-sink': 'test:blob-sink/blob-sink@0.1.0',
  'tensor-source': 'test:tensor-source/tensor-source@0.1.0',
  'tensor-invert': 'test:tensor-invert/tensor-invert@0.1.0',
  'tensor-copy-invert': 'test:tensor-copy-invert/tensor-copy-invert@0.1.0',
  'tensor-sink': 'test:tensor-sink/tensor-sink@0.1.0',
  'tensor-keep': 'test:tensor-keep/tensor-keep@0.1.0',
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
    nodes: nodes.map(([id, guest]) => ({
      id,
      component: componentRef(inline[guest]?.id ?? ids[guest]),
    })),
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
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
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
  assert.deepEqual(
    runs().map((e: any) => e.node),
    ['a', 'b'],
  );
  assert.equal(session.graph.readOutput('b', 'out'), 6.5);

  assert.throws(() => session.graph.inject('a', 'in', 'five'), /expected a number/);
  assert.throws(() => session.graph.inject('b', 'in', 1), /written by connection/);
  // The connection is the error, whatever the value.
  assert.throws(() => session.graph.inject('b', 'in', 'five'), /written by connection/);
  assert.throws(() => session.graph.inject('a', 'nope', 1), /not a Value input port/);
  assert.throws(() => session.graph.readOutput('x', 'out'), /unknown node `x`/);
  assert.throws(() => session.graph.readOutput('a', 'nope'), /not a Value or handle output port/);
});

test('a stream passes between two nodes of an island, and a tap sees its items', async () => {
  const seen: StreamItem[] = [];
  const session = await load(
    [
      ['prod', 'stream-producer'],
      ['cons', 'stream-consumer'],
    ],
    [['s', 'prod.items', 'cons.items']],
    { onStreamItem: (item) => seen.push(item) },
  );
  assert.deepEqual(session.graph.islands(), [['prod', 'cons']]);
  session.graph.inject('prod', 'burst-size', 5);
  await session.settle();
  assert.equal(session.graph.readOutput('cons', 'count'), 5);
  assert.equal(session.graph.readOutput('cons', 'total'), 10);
  assert.equal(session.graph.readOutput('prod', 'limit'), 5);
  const items = seen.flatMap(({ node, port, item, items }) => {
    assert.deepEqual([node, port], ['prod', 'items']);
    // A chunk of numbers is a typed array: its items, not copied.
    assert.equal(items, item);
    return Array.from(items);
  });
  assert.deepEqual(items, [0, 1, 2, 3, 4]);
});

/** `test:video/clock`, sleeping on the event loop. */
const clock = (_node: string, capability: Capability) =>
  capability.interface === 'test:video/clock@0.1.0'
    ? { sleep: (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)) }
    : undefined;

/** A frame as jco hands one over. */
interface Frame {
  width: number;
  height: number;
  pixels: Uint8Array;
}

test('a stream of records reaches the tap one record a chunk, and outputs carry their viewer', async () => {
  const seen: StreamItem[] = [];
  const session = await load(
    [
      ['source', 'video-source'],
      ['gray', 'frame-grayscale'],
      ['sink', 'video-sink'],
    ],
    [
      ['a', 'source.frames', 'gray.frames'],
      ['b', 'gray.frames', 'sink.frames'],
    ],
    { capabilities: clock, onStreamItem: (item) => seen.push(item) },
  );
  session.graph.inject('source', 'frames', 3);
  session.graph.inject('source', 'fps', 100);
  await session.settle();
  assert.equal(session.graph.readOutput('sink', 'count'), 3);
  assert.equal(session.graph.readOutput('sink', 'gray'), true);

  for (const node of ['source', 'gray']) {
    const chunks = seen.filter((s) => s.node === node);
    assert.equal(chunks.length, 3, node);
    for (const { port, item, items } of chunks) {
      assert.equal(port, 'frames');
      assert.deepEqual(Array.from(items), [item], 'a record is one item');
      const frame = item as Frame;
      assert.deepEqual([frame.width, frame.height], [96, 64]);
      assert.ok(frame.pixels instanceof Uint8Array);
      assert.equal(frame.pixels.length, 96 * 64 * 4);
    }
  }
  const rgb = (node: string) => {
    const { pixels } = seen.find((s) => s.node === node)!.item as Frame;
    // Row 40, column 50: below the square of the first frame.
    const at = (40 * 96 + 50) * 4;
    return [...pixels.subarray(at, at + 3)];
  };
  const [r, g, b] = rgb('gray');
  assert.ok(r === g && g === b, `gray: ${[r, g, b]}`);
  const [cr, cg, cb] = rgb('source');
  assert.ok(cr !== cg || cg !== cb, `the source is in colour: ${[cr, cg, cb]}`);

  // The bytes' `witgraph-viewer` section, on the contract.
  const viewers = (node: string) =>
    session.contracts.get(node)!.outputs.map(({ name, viewer }) => [name, viewer]);
  assert.deepEqual(viewers('source'), [['frames', 'image']]);
  assert.deepEqual(viewers('gray'), [['frames', 'image']]);
  assert.ok(viewers('sink').every(([, viewer]) => viewer === undefined));
});

test('a contract from WIT text takes its viewers from the bytes', async () => {
  const wit = await readFile(
    new URL('test-fixtures/test-components/guests/frame-grayscale/wit/world.wit', root),
    'utf8',
  );
  const source = await guestSource('video-source');
  const { bytes } = await guestSource('frame-grayscale');
  const graph = {
    metadata: { name: 'test' },
    components: [componentRef(ids['video-source']), componentRef(ids['frame-grayscale'])],
    nodes: [
      { id: 'source', component: componentRef(ids['video-source']) },
      { id: 'gray', component: componentRef(ids['frame-grayscale']) },
    ],
    connections: [
      { id: 'a', from: { node: 'source', port: 'frames' }, to: { node: 'gray', port: 'frames' } },
    ],
  };
  const session = await loaded({
    graph,
    components: [source, { id: ids['frame-grayscale'], bytes, wit }],
    wasm,
    capabilities: clock,
  });
  assert.equal(session.contracts.get('gray')!.outputs[0].viewer, 'image');
});

test('a wrong viewer hint is a warning, not a failed load', async () => {
  // `video-source` with a section at its root naming an output it lacks.
  const { id, bytes } = await guestSource('video-source');
  const name = new TextEncoder().encode('witgraph-viewer');
  const hint = new TextEncoder().encode('output.frame=image\n');
  const body = [name.length, ...name, ...hint];
  const stale = Uint8Array.from([...bytes, 0, body.length, ...body]);
  const warned: unknown[] = [];
  const warn = console.warn;
  console.warn = (...args: unknown[]) => warned.push(args.join(' '));
  try {
    const session = await loaded({
      graph: {
        metadata: { name: 'test' },
        components: [componentRef(id)],
        nodes: [{ id: 'source', component: componentRef(id) }],
        connections: [],
      },
      components: [{ id, bytes: stale }],
      wasm,
      capabilities: clock,
    });
    assert.equal(session.warnings.length, 1);
    assert.match(session.warnings[0], /`test:video-source.*`frame`/);
    assert.deepEqual(warned, [`witgraph: ${session.warnings[0]}`]);
    // Its good hint still holds.
    assert.equal(session.contracts.get('source')!.outputs[0].viewer, 'image');
  } finally {
    console.warn = warn;
  }
});

test('disposing a session stops the streams its nodes are still writing', async () => {
  let frames = 0;
  const session = await load(
    [
      ['source', 'video-source'],
      ['sink', 'video-sink'],
    ],
    [['a', 'source.frames', 'sink.frames']],
    { capabilities: clock, onStreamItem: () => frames++ },
  );
  // Endless, paced: the tick lasts as long as the stream.
  session.graph.inject('source', 'fps', 200);
  const tick = session.tick().catch(() => undefined);
  while (frames < 3) await new Promise((resolve) => setTimeout(resolve, 5));
  session.dispose();
  await tick;
  const after = frames;
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(frames, after, 'no frame is seen once the session is disposed');
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
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
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
  const session = await load([
    ['busy', 'busy-loop'],
    ['e', 'echo'],
  ]);
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
  const feed = [Uint8Array.of(3), Uint8Array.of(4, 5), Uint8Array.of(6, 7, 8)];
  const asked: [string, Capability][] = [];
  const session = await load(
    [
      ['mqtt', 'mqtt-node'],
      ['cons', 'bytes-consumer'],
      ['kv', 'labelled'],
      ['busy', 'busy-loop'],
    ],
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
  assert.equal(session.graph.readOutput('cons', 'count'), 3);
  assert.equal(session.graph.readOutput('cons', 'total-bytes'), 6);
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
  const session = await load(
    [
      ['a', 'echo'],
      ['b', 'echo'],
    ],
    [['c', 'a.out', 'b.in']],
    {
      beforeRun: (node) =>
        node === 'b'
          ? new Promise<void>((resolve) => {
              resume = resolve;
              paused();
            })
          : undefined,
    },
  );
  session.graph.inject('a', 'in', 7);
  const tick = session.tick();
  await pause;
  assert.equal(session.graph.readOutput('a', 'out'), 7, 'what `a` returned');
  assert.equal(session.graph.readOutputWave('a', 'out'), '7');
  assert.equal(session.graph.readOutput('b', 'out'), undefined, '`b` has not returned');
  assert.equal(session.graph.nodeState('b').phase, 'running');
  assert.ok(
    session.graph.takeTrace().some((e: any) => e.event === 'run-started' && e.node === 'b'),
  );
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
    beforeRun: () =>
      new Promise<void>((resolve) => {
        resume = resolve;
        paused();
      }),
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
    [
      { id: ids.echo, bytes: maybe.bytes },
      { id: `${ids.echo}#${hash}`, bytes: echo.bytes },
    ],
    [
      { id: `${ids.echo}#${hash}`, bytes: echo.bytes },
      { id: ids.echo, bytes: maybe.bytes },
    ],
  ]) {
    const session = await loaded({ graph, components, wasm });
    assert.deepEqual(
      session.graph.component('n').index,
      components.findIndex((c) => c.bytes === echo.bytes),
    );
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
  assert.throws(
    () => session.graph.inject('n', 'b', 3),
    /expected `\{ tag: 'none' \| 'some', val \}`/,
  );
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
  const session = await load(
    [
      ['n', 'values'],
      ['m', 'values'],
    ],
    [],
    {
      mocks: { m: () => ({ a: { tag: 'none' }, b: undefined, xs: { length: Infinity } }) },
    },
  );
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
    [
      ['p1', 'stream-producer'],
      ['c1', 'stream-consumer'],
      ['m', 'stream-producer'],
      ['c2', 'stream-consumer'],
    ],
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
    [
      ['mqtt', 'mqtt-node'],
      ['cons', 'bytes-consumer'],
    ],
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
  assert.match(
    fault.fault.message,
    /`mqtt`: node `mqtt` imports `test:mqtt\/source@0.1.0`, which nothing provides/,
  );
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
    rebuild: () => {
      rebuilding.open();
      return new Promise(() => {});
    },
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
      Object.assign(this, {
        scaled: function (this: Mocks, inputs: any) {
          return { out: inputs.in * this.factor };
        },
      });
    }
  }
  const after: [string, string[]][] = [];
  const session = await load(
    [
      ['constructor', 'echo'],
      ['toString', 'echo'],
      ['scaled', 'echo'],
      ['none', 'echo'],
    ],
    [],
    {
      mocks: Object.assign(new Mocks() as any, { none: () => undefined }),
      afterRun: (node, outputs) => void after.push([node, Object.keys(outputs)]),
    },
  );
  for (const node of ['constructor', 'toString', 'scaled', 'none'])
    session.graph.inject(node, 'in', 2);
  await session.settle();
  assert.deepEqual(
    session.graph.takeFaults().map((f: any) => f.members),
    [['none']],
    'only the empty mock is short of `out`',
  );
  assert.equal(session.graph.readOutput('constructor', 'out'), 2, 'not mocked: it ran');
  assert.equal(session.graph.readOutput('toString', 'out'), 2, 'not mocked: it ran');
  assert.equal(session.graph.readOutput('scaled', 'out'), 20);
  assert.deepEqual(
    after.find(([node]) => node === 'none'),
    ['none', []],
  );
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
      connections: [
        { id: 's', from: { node: 'prod', port: 'items' }, to: { node: 'cons', port: 'items' } },
      ],
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

test('a Value is passed on as its producer returned it, and compared without converting', async () => {
  const runs: string[] = [];
  const session = await load(
    [
      ['source', 'blob-source'],
      ['a', 'blob-relay'],
      ['sink', 'blob-sink'],
    ],
    [
      ['s-a', 'source.blob', 'a.blob'],
      ['a-s', 'a.blob', 'sink.blob'],
    ],
    { afterRun: (node) => void runs.push(node) },
  );
  session.graph.inject('source', 'size', 1000);
  session.graph.inject('source', 'seed', 7);
  await session.settle();
  assert.deepEqual(runs, ['source', 'a', 'sink']);
  const blob = session.graph.readOutput('source', 'blob');
  assert.ok(blob instanceof Uint8Array);
  assert.equal(blob.length, 1000);
  assert.equal(blob[3], 10);
  // The very value, shared, not a copy made for each read.
  assert.equal(session.graph.readOutput('source', 'blob'), blob);
  assert.equal(session.graph.readOutput('sink', 'len'), 1000);
  // WAVE text is made when asked for.
  assert.equal(session.graph.readOutputWave('sink', 'len'), '1000');
  assert.match(session.graph.readOutputWave('a', 'blob')!, /^\[7, 8, 9, /);

  // An equal value runs nothing downstream; another one does.
  runs.length = 0;
  session.graph.inject('source', 'size', 1000);
  session.graph.rerun('source');
  await session.settle();
  assert.deepEqual(runs, ['source'], 'the same bytes again: `a` does not rerun');
  runs.length = 0;
  session.graph.inject('source', 'seed', 8);
  await session.settle();
  assert.deepEqual(runs, ['source', 'a', 'sink']);
  assert.equal((session.graph.readOutput('a', 'blob') as Uint8Array)[0], 8);
});

test('the list loop jco generates is there to be replaced', async () => {
  // jco lifts a list a guest returns an element at a time; the loader
  // copies a list of numbers at once instead. A jco that generates its
  // loop otherwise leaves it alone, and this fails, so the copy is redone.
  const { bytes } = await guestSource('blob-relay');
  const { files } = await transpile(bytes, {
    name: 'node',
    instantiation: { tag: 'async' },
    noTypescript: true,
  });
  const source = new TextDecoder().decode(
    (files as [string, Uint8Array][]).find(([name]) => name.endsWith('.js'))![1],
  );
  assert.notEqual(fastListLift(source), source);
  assert.equal(fastListLift('nothing to replace'), 'nothing to replace');
});

/** Whether the scheduler's module has a 64-bit memory (`pnpm build:wasm64`). */
function memory64(module: Uint8Array): boolean {
  const leb = (at: number): [number, number] => {
    let value = 0;
    for (let shift = 0; ; shift += 7) {
      const byte = module[at++];
      value += (byte & 0x7f) * 2 ** shift;
      if (byte < 0x80) return [value, at];
    }
  };
  // Sections after the 8-byte header: the memory section's first limits.
  for (let at = 8; at < module.length; ) {
    const id = module[at];
    const [size, body] = leb(at + 1);
    if (id === 5) return (module[leb(body)[1]] & 0x04) !== 0;
    at = body + size;
  }
  return false;
}

test('a value too large to convert is refused, naming it, and the session goes on', async () => {
  const session = await load(
    [
      ['source', 'blob-source'],
      ['sink', 'blob-sink'],
    ],
    [['c', 'source.blob', 'sink.blob']],
  );
  const size = 64 * 1024 * 1024;
  session.graph.inject('source', 'size', size);
  await session.settle();
  if (memory64(wasm)) {
    // A 64-bit memory affords it.
    assert.match(session.graph.readOutputWave('source', 'blob')!, /^\[0, 1, 2, /);
    return;
  }
  // As WAVE text it would not fit the scheduler's 32-bit memory.
  assert.throws(
    () => session.graph.readOutputWave('source', 'blob'),
    /`source\.blob`: too large to convert/,
  );
  // A snapshot names the first such value it meets: the output, or the
  // sink's latched input holding the same bytes.
  assert.throws(() => session.graph.snapshot(), /`(source|sink)\.blob`: too large to convert/);
  // Nothing trapped: the session is as it was.
  assert.equal(session.broken, undefined);
  assert.equal((session.graph.readOutput('source', 'blob') as Uint8Array).length, size);
  assert.equal(session.graph.readOutputWave('sink', 'len'), String(size));
  session.graph.inject('source', 'size', 3);
  await session.settle();
  assert.equal(session.graph.readOutputWave('source', 'blob'), '[0, 1, 2]');
  assert.ok(session.graph.snapshot().length > 0);
});

test('a trap of the scheduler breaks the session, naming the call', async () => {
  const state: { broken?: Error } = {};
  const graph = watchTraps(
    {
      readOutputWave(node: string, port: string): string {
        throw new WebAssembly.RuntimeError(`unreachable (${node}.${port})`);
      },
      readOutput: () => 3,
      parse: () => {
        throw new Error('not a trap');
      },
      tick: async () => {
        throw new WebAssembly.RuntimeError('unreachable');
      },
    },
    state,
  );
  // Other errors pass through, and leave the session as it is.
  assert.throws(() => graph.parse(), /^Error: not a trap$/);
  assert.equal(graph.readOutput(), 3);
  assert.throws(
    () => graph.readOutputWave('source', 'blob'),
    /the scheduler stopped in `readOutputWave\("source", "blob"\)` \(unreachable \(source\.blob\)\): the session can no longer be used/,
  );
  // Every call after throws the same.
  assert.equal(state.broken?.cause instanceof WebAssembly.RuntimeError, true);
  assert.throws(
    () => graph.readOutput(),
    (e) => e === state.broken,
  );

  const other: { broken?: Error } = {};
  await assert.rejects(
    watchTraps(
      {
        tick: async () => {
          throw new WebAssembly.RuntimeError('unreachable');
        },
      },
      other,
    ).tick(),
    /stopped in `tick\(\)`/,
  );
  assert.ok(other.broken);
});

test('a trap of the scheduler during a tick fails the tick and breaks every session', async () => {
  const session = await load(
    [
      ['prod', 'stream-producer'],
      ['cons', 'stream-consumer'],
    ],
    [['s', 'prod.items', 'cons.items']],
  );
  const bystander = await load([['a', 'echo']]);
  // Endless: the tick runs until something stops it.
  const tick = session.tick();
  await new Promise((resolve) => setTimeout(resolve, 20));
  const trap = (where: string) => {
    const error = new WebAssembly.RuntimeError('unreachable');
    error.stack = `RuntimeError: unreachable\n    at ${where}`;
    // What the host reports for an exception nothing caught.
    process.emit('uncaughtExceptionMonitor', error, 'uncaughtException');
  };
  // A guest's trap is its node's fault, not the scheduler's.
  trap('relay_guest.wasm.run (wasm://wasm/relay_guest.wasm-1:1)');
  assert.equal(session.broken, undefined);
  trap('witgraph_web.wasm.alloc (wasm://wasm/witgraph_web.wasm-1:1)');
  await assert.rejects(tick, /the scheduler stopped while running a tick \(unreachable\)/);
  for (const s of [session, bystander]) {
    assert.match(String(s.broken), /load the graph again/);
    assert.throws(
      () => s.graph.readOutput('cons', 'count'),
      (e) => e === s.broken,
    );
  }
  // Disposing a broken session leaves what the trap held alone.
  session.dispose();
  bystander.dispose();
});

test('a hook sees what a future resolves to, and its reader still gets it', async () => {
  const seen: FutureItem[] = [];
  const session = await load(
    [
      ['w', 'future-writer'],
      ['r', 'future-reader'],
      ['alone', 'future-writer'],
    ],
    [['c', 'w.done', 'r.data']],
    { onFutureValue: (item) => seen.push(item) },
  );
  await session.settle();
  assert.equal(session.graph.readOutput('r', 'value'), 7);
  // The unread one too, once it settles.
  for (let i = 0; i < 50 && seen.length < 2; i++) await new Promise((r) => setTimeout(r, 10));
  const values = seen.map(({ node, port, value }) => [node, port, value]).sort();
  assert.deepEqual(values, [
    ['alone', 'done', 7],
    ['w', 'done', 7],
  ]);
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

/** `calc` nodes, each linked to `math` for `test:math/ops`; `math` is a provider, not a node. */
async function loadLinked(
  nodes: string[],
  { providers = ['math'] }: { providers?: string[] } = {},
): Promise<Session> {
  const components = await Promise.all(['calc', ...providers].map(guestSource));
  const ops = 'test:math/ops@0.1.0';
  const graph = {
    metadata: { name: 'test' },
    components: [componentRef(ids.calc)],
    nodes: nodes.map((id) => ({ id, component: componentRef(ids.calc) })),
    connections: [],
    links: nodes.map((id) => ({
      id: `link-${id}`,
      node: id,
      import: ops,
      provider: componentRef(ids.math),
      export: ops,
    })),
  };
  return loaded({ graph, components, wasm });
}

test('a node runs with the provider its link names composed in', async () => {
  // No `capabilities`: the link satisfies the only import `calc` has.
  const session = await loadLinked(['n']);
  session.graph.inject('n', 'x', 3);
  await session.settle();
  assert.equal(session.graph.readOutput('n', 'doubled'), 6);
  assert.equal(session.graph.readOutput('n', 'slow'), 6);
  assert.equal(session.graph.readOutput('n', 'total'), 3, '0 + 1 + 2, streamed by the provider');
  assert.equal(session.graph.readOutput('n', 'calls'), 1);
});

test('each node gets an instance of the provider of its own', async () => {
  const session = await loadLinked(['a', 'b']);
  session.graph.inject('a', 'x', 2);
  session.graph.inject('b', 'x', 5);
  await session.settle();
  assert.equal(session.graph.readOutput('a', 'doubled'), 4);
  assert.equal(session.graph.readOutput('b', 'doubled'), 10);
  assert.equal(session.graph.readOutput('a', 'calls'), 1);
  assert.equal(
    session.graph.readOutput('b', 'calls'),
    1,
    'not the 2 a shared instance would count',
  );
});

test('a node component is composed once per set of links', async () => {
  const session = await loadLinked(['a', 'b']);
  const [a, b] = ['a', 'b'].map(
    (node) =>
      session.graph.component(node) as { index: number; sha256: string; bytes?: Uint8Array },
  );
  assert.ok(a.bytes && b.bytes, 'both come back composed');
  assert.equal(a.sha256, b.sha256);
  assert.equal(a.index, 0, 'index still names the node entry, not the provider');
});

test('a link whose provider was not given fails the load by name', async () => {
  await assert.rejects(loadLinked(['n'], { providers: [] }), /test:math\/math@0\.1\.0/);
});

/**
 * `test:data/tensor` in host memory, as every node imports it: one class,
 * so a tensor one node makes is one another accepts. Counts how often each
 * tensor is disposed.
 */
function cpuTensors() {
  const disposed = new Map<object, number>();
  const made: object[] = [];
  class Tensor {
    readonly #dtype: string;
    readonly #shape: number[];
    readonly #data: Uint8Array;
    constructor(dtype: string, shape: ArrayLike<number>, data: Uint8Array) {
      this.#dtype = dtype;
      this.#shape = Array.from(shape);
      this.#data = data;
      made.push(this);
    }
    static fromBytes(dtype: string, shape: ArrayLike<number>, data: Uint8Array) {
      return new Tensor(dtype, shape, data.slice());
    }
    dtype() {
      return this.#dtype;
    }
    shape() {
      return this.#shape;
    }
    read(offset: bigint, len: bigint) {
      return this.#data.slice(Number(offset), Number(offset + len));
    }
    clone() {
      // Its own object: jco disposes it when the guest drops it.
      return new Tensor(this.#dtype, this.#shape, this.#data);
    }
    invert() {
      return new Tensor(
        this.#dtype,
        this.#shape,
        this.#data.map((b) => 255 - b),
      );
    }
    conv3x3(): never {
      throw new Error('not on the CPU');
    }
    [Symbol.dispose]() {
      disposed.set(this, (disposed.get(this) ?? 0) + 1);
    }
  }
  const live = () => made.filter((t) => !disposed.has(t)).length;
  return { imports: { Tensor }, disposed, made, live };
}

test('tensors pass between nodes as the objects their makers returned, disposed once unheld', async () => {
  const tensors = cpuTensors();
  const session = await load(
    [
      ['src', 'tensor-source'],
      ['inv', 'tensor-invert'],
      ['copy', 'tensor-copy-invert'],
      ['direct', 'tensor-sink'],
      ['inv-sink', 'tensor-sink'],
      ['copy-sink', 'tensor-sink'],
    ],
    [
      ['a', 'src.tensor', 'inv.tensor'],
      ['b', 'src.tensor', 'copy.tensor'],
      ['c', 'src.tensor', 'direct.tensor'],
      ['d', 'inv.tensor', 'inv-sink.tensor'],
      ['e', 'copy.tensor', 'copy-sink.tensor'],
    ],
    {
      capabilities: (_, capability) =>
        capability.interface === 'test:data/tensor@0.1.0' ? tensors.imports : undefined,
    },
  );
  const [input] = session.contracts.get('inv')?.inputs ?? [];
  assert.equal(input?.kind, 'handle');
  assert.deepEqual(input?.handle, {
    interface: 'test:data/tensor@0.1.0',
    resource: 'tensor',
  });
  await session.settle();

  const bytes = 64 * 48 * 4;
  const sum = session.graph.readOutput('direct', 'sum') as bigint;
  assert.equal(session.graph.readOutput('direct', 'len'), BigInt(bytes));
  const inverted = 255n * BigInt(bytes) - sum;
  assert.equal(session.graph.readOutput('inv-sink', 'sum'), inverted, 'inverted by the host');
  assert.equal(session.graph.readOutput('copy-sink', 'sum'), inverted, 'inverted by the guest');
  const source = session.graph.readOutput('src', 'tensor');
  assert.ok(source instanceof tensors.imports.Tensor, 'the object its maker returned');
  assert.throws(() => session.graph.readOutputWave('src', 'tensor'), /has no WAVE text/);
  assert.throws(() => session.graph.inject('inv', 'tensor', source), /not a Value input port/);
  assert.equal(tensors.live(), 3, 'the source tensor and the two inverted ones');

  session.graph.inject('src', 'seed', 7);
  await session.settle();
  assert.notEqual(session.graph.readOutput('direct', 'sum'), sum);
  assert.equal(tensors.live(), 3, 'the old ones are disposed');
  assert.ok(
    [...tensors.disposed.values()].every((times) => times === 1),
    'each once, borrows by readers never',
  );
  session.dispose();
  sessions.splice(sessions.indexOf(session), 1);
  assert.equal(tensors.live(), 0, 'a disposed session holds none');
});

test('a node keeps its own reference to a tensor across runs', async () => {
  const tensors = cpuTensors();
  const session = await load(
    [
      ['src', 'tensor-source'],
      ['keep', 'tensor-keep'],
    ],
    [['a', 'src.tensor', 'keep.tensor']],
    {
      capabilities: (_, capability) =>
        capability.interface === 'test:data/tensor@0.1.0' ? tensors.imports : undefined,
    },
  );
  await session.settle();
  assert.equal(session.graph.readOutput('keep', 'previous'), 0n);
  session.graph.inject('src', 'seed', 1);
  await session.settle();
  assert.ok((session.graph.readOutput('keep', 'previous') as bigint) > 0n);
  // The latched second tensor, and `keep`'s clone of it.
  assert.equal(tensors.live(), 2);
});
