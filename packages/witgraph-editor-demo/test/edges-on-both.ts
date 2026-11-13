// Loaded in the page, through Vite, by accelerators.spec.ts.
import { monotonicClock } from '@bytecodealliance/preview3-shim/clocks';
import { type Capability, Session } from 'witgraph-loader';
import { cpu } from '/src/accel/cnn.ts';
import { webgpu } from '/src/accel/webgpu.ts';

const ref = (name: string) => ({ component: name, world: `test:${name}/${name}@0.1.0` });

const GUESTS = ['video-source', 'edge-detect', 'edge-detect-webgpu', 'video-sink'];

/** A frame as jco hands one over. */
interface Frame {
  pixels: Uint8Array;
}

/**
 * The same frames through `edge-detect` on the CPU, the reference, and
 * through `edge-detect-webgpu`: the largest difference in any pixel value,
 * or `Infinity` when they did not make the same frames.
 */
export async function edgesOnBoth(frames: number): Promise<number> {
  const seen: Record<string, Frame[]> = { cpu: [], gpu: [] };
  const components = await Promise.all(
    GUESTS.map(async (guest) => {
      const bytes = async (file: string) =>
        new Uint8Array(await (await fetch(`/components/${file}`)).arrayBuffer());
      return {
        name: guest,
        manifest: await (await fetch(`/components/${guest}.node.json`)).json(),
        wit: await bytes(`${guest}.wit.wasm`),
        bytes: await bytes(`${guest}.wasm`),
      };
    }),
  );
  const chain = (id: string, edges: string) => [
    { id: `${id}-source`, ...ref('video-source') },
    { id, ...ref(edges) },
    { id: `${id}-sink`, ...ref('video-sink') },
  ];
  const wires = (id: string) => [
    {
      id: `${id}-in`,
      from: { node: `${id}-source`, port: 'frames' },
      to: { node: id, port: 'frames' },
    },
    {
      id: `${id}-out`,
      from: { node: id, port: 'edges' },
      to: { node: `${id}-sink`, port: 'frames' },
    },
  ];
  const session = await Session.load({
    graph: {
      metadata: { name: 'edges-on-both' },
      components: GUESTS.map(ref),
      nodes: [...chain('cpu', 'edge-detect'), ...chain('gpu', 'edge-detect-webgpu')],
      connections: [...wires('cpu'), ...wires('gpu')],
    },
    components,
    capabilities: (capability: Capability) => {
      if (capability.interface.startsWith('wasi:clocks/monotonic-clock')) return monotonicClock;
      if (capability.interface.startsWith('test:accel/cnn')) return cpu();
      if (capability.interface.startsWith('wasi:webgpu/webgpu')) return webgpu;
    },
    onStreamItem: ({ node, port, item }: { node: string; port: string; item: unknown }) => {
      if (port === 'edges') seen[node].push(item as Frame);
    },
  } as never);
  await session.graph.inject('cpu-source', 'frames', frames);
  await session.graph.inject('gpu-source', 'frames', frames);
  await session.settle();
  session.dispose();
  if (seen.cpu.length !== frames || seen.gpu.length !== frames) return Infinity;
  let largest = 0;
  for (const [i, { pixels }] of seen.cpu.entries()) {
    for (const [at, value] of pixels.entries()) {
      largest = Math.max(largest, Math.abs(value - seen.gpu[i].pixels[at]));
    }
  }
  return largest;
}
