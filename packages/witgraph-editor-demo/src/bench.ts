// Spike: how fast frames move through a stream of handles in a browser.
import { type Capability, type ComponentSource, Session } from 'witgraph-loader';
import { camera } from './data/camera.ts';
import { type Backend, cpu as cpuTensors, tensors } from './data/tensors.ts';
import { webgpu } from './data/webgpu.ts';
import { webnn } from './data/webnn.ts';

const ids = (guest: string) => `test:${guest}/${guest}@0.1.0`;
const ref = (guest: string) => `test:${guest}/${guest}@0.1.0`;

async function component(guest: string): Promise<ComponentSource> {
  const response = await fetch(`/components/${guest}.wasm`);
  if (!response.ok) throw new Error(`no ${guest}`);
  return { id: ids(guest), bytes: new Uint8Array(await response.arrayBuffer()) };
}

async function session(
  nodes: [string, string][],
  edges: [string, string][],
  capabilities?: (node: string, capability: Capability) => object | undefined,
): Promise<Session> {
  const guests = [...new Set(nodes.map(([, guest]) => guest))];
  const at = (text: string) => {
    const [node, port] = text.split('.');
    return { node, port };
  };
  const refOf = (guest: string) => {
    const [, pkg, world] = ref(guest).match(/^test:([^/]+)\/([^@]+)@/) ?? [];
    return { package: { namespace: 'test', name: pkg, version: '0.1.0' }, world };
  };
  return Session.load({
    graph: {
      metadata: { name: 'bench' },
      components: guests.map(refOf),
      nodes: nodes.map(([id, guest]) => ({ id, component: refOf(guest) })),
      connections: edges.map(([from, to], i) => ({ id: `e${i}`, from: at(from), to: at(to) })),
    },
    components: await Promise.all(guests.map(component)),
    capabilities,
  } as never);
}

/** `backend`, counting what it holds. */
function counted(backend: Backend) {
  let live = 0;
  const held = <T>(p: Promise<T>) =>
    p.then((v) => {
      live += 1;
      return v;
    });
  const wrapped: Backend = {
    ...backend,
    upload: (...a) => held(backend.upload(...a)),
    invert: (...a) => held(backend.invert(...a)),
    conv3x3: (...a) => held(backend.conv3x3(...a)),
    capture: backend.capture && ((...a) => held(backend.capture!(...a))),
    destroy: (s) => {
      live -= 1;
      backend.destroy(s);
    },
  };
  return { backend: wrapped, live: () => live };
}

const makers: Record<string, () => Promise<Backend>> = {
  cpu: async () => cpuTensors(),
  webgpu,
  webnn,
};

async function adapter(): Promise<string> {
  const a = await (navigator as any).gpu?.requestAdapter();
  const info = a?.info;
  return info ? `${info.vendor}/${info.architecture}/${info.device}/${info.description}` : 'none';
}

interface Case {
  backend: string;
  width: number;
  height: number;
  frames: number;
  reuse: boolean;
  readLast: boolean;
  /** How many invert nodes to chain (1 when absent). */
  filters?: number;
}

/** Milliseconds a frame, the median of `runs`, after one warming up. */
async function timed(s: Session, run: (n: number) => void, frames: number, runs = 3) {
  const times: number[] = [];
  for (let i = 0; i <= runs; i++) {
    const n = i === 0 ? Math.min(5, frames - 1) : frames + i;
    run(n);
    const start = performance.now();
    await s.settle();
    if (i > 0) times.push((performance.now() - start) / n);
  }
  times.sort((a, b) => a - b);
  return times[Math.floor(times.length / 2)];
}

async function handles(c: Case) {
  const { backend, live } = counted(await makers[c.backend]());
  backend.name();
  const imports = tensors(backend);
  const filters = c.filters ?? 1;
  const invert = Array.from({ length: filters }, (_, i): [string, string] => [
    `inv${i}`,
    'tensor-stream-invert',
  ]);
  const path = ['src', ...invert.map(([id]) => id), 'sink'];
  const s = await session(
    [['src', 'tensor-stream-source'], ...invert, ['sink', 'tensor-stream-sink']],
    path.slice(1).map((to, i): [string, string] => [`${path[i]}.tensors`, `${to}.tensors`]),
    (_, cap) => (cap.interface.startsWith('test:data/tensor') ? imports : undefined),
  );
  s.graph.inject('src', 'width', c.width);
  s.graph.inject('src', 'height', c.height);
  s.graph.inject('src', 'reuse', c.reuse);
  s.graph.inject('sink', 'read-last', c.readLast);
  const ms = await timed(s, (n) => s.graph.inject('sink', 'take', n), c.frames);
  const count = s.graph.readOutput('sink', 'count');
  s.dispose();
  for (let i = 0; i < 100 && live() > 0; i++) await new Promise((r) => setTimeout(r, 20));
  return { ...c, msPerFrame: Number(ms.toFixed(2)), count, liveAfter: live() };
}

/** The page's camera through edges and invert, as the demo's graph has it. */
async function cameraChain(c: { backend: string; frames: number }) {
  const { backend, live } = counted(await makers[c.backend]());
  backend.name();
  const imports = tensors(backend);
  const source = camera(backend, imports.Tensor);
  const s = await session(
    [
      ['camera', 'camera-source'],
      ['edges', 'tensor-stream-edge'],
      ['invert', 'tensor-stream-invert'],
      ['sink', 'tensor-stream-sink'],
    ],
    [
      ['camera.tensors', 'edges.tensors'],
      ['edges.tensors', 'invert.tensors'],
      ['invert.tensors', 'sink.tensors'],
    ],
    (_, cap) =>
      cap.interface.startsWith('test:camera/camera')
        ? source
        : cap.interface.startsWith('test:data/tensor')
          ? imports
          : undefined,
  );
  s.graph.inject('sink', 'read-last', true);
  const ms = await timed(s, (n) => s.graph.inject('sink', 'take', n), c.frames);
  const count = s.graph.readOutput('sink', 'count');
  s.dispose();
  for (let i = 0; i < 100 && live() > 0; i++) await new Promise((r) => setTimeout(r, 20));
  return { camera: true, ...c, msPerFrame: Number(ms.toFixed(2)), count, liveAfter: live() };
}

async function copied(frames: number) {
  const clock = { sleep: (ms: number) => new Promise((r) => setTimeout(r, ms)) };
  const s = await session(
    [
      ['src', 'video-source'],
      ['gray', 'frame-grayscale'],
      ['sink', 'video-sink'],
    ],
    [
      ['src.frames', 'gray.frames'],
      ['gray.frames', 'sink.frames'],
    ],
    () => clock,
  );
  const ms = await timed(s, (n) => s.graph.inject('src', 'frames', n), frames);
  s.dispose();
  return { control: 'copied 96x64 frames', msPerFrame: Number(ms.toFixed(2)) };
}

async function numbers(items: number) {
  const s = await session(
    [
      ['p', 'stream-producer'],
      ['c', 'stream-consumer'],
    ],
    [['p.items', 'c.items']],
  );
  const ms = await timed(s, (n) => s.graph.inject('p', 'burst-size', n), items);
  s.dispose();
  return { control: 'u32 through a stream', msPerFrame: Number(ms.toFixed(2)) };
}

Object.assign(globalThis, { bench: { handles, cameraChain, copied, numbers, adapter } });
document.title = 'ready';
