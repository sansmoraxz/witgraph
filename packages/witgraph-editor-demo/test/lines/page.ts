// A page for `lines.spec.ts`: a session in a worker, driven from here.
import { type LineDebugger, type LineStop, RemoteSession } from 'witgraph-loader';

let frames = 0;
const frame = () => {
  frames += 1;
  requestAnimationFrame(frame);
};
requestAnimationFrame(frame);
const live = document.querySelector('#live') as HTMLButtonElement;
live.onclick = () => {
  live.textContent = String(Number(live.textContent) + 1);
};

const fetched = async (url: URL) => new Uint8Array(await (await fetch(url)).arrayBuffer());
const stops: LineStop[] = [];
const ref = { component: 'line-walk', world: 'test:line-walk/line-walk@0.1.0' };
const session = await RemoteSession.load(
  new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' }),
  {
    graph: {
      metadata: { name: 'lines' },
      components: { 'line-walk': 'line-walk' },
      nodes: [{ id: 'walker', ...ref }],
      connections: [],
    },
    components: [
      {
        name: 'line-walk',
        manifest: await (
          await fetch(new URL('../../components/line-walk.node.json', import.meta.url))
        ).json(),
        wit: await fetched(new URL('../../components/line-walk.wit.wasm', import.meta.url)),
        bytes: await fetched(new URL('../../components/line-walk.wasm', import.meta.url)),
      },
    ],
    wasm: await fetched(
      new URL('../../../witgraph-loader/pkg/witgraph_web_bg.wasm', import.meta.url),
    ),
    lines: { onStop: (stop) => stops.push(stop) },
  },
);
const lines = session.lines as LineDebugger;
const file = lines.files.find((name) => name.endsWith('line-walk/src/lib.rs')) as string;
let ticked: unknown;

Object.assign(window, {
  ready: true,
  dbg: {
    isolated: () => crossOriginIsolated,
    frames: () => frames,
    stops: () => stops,
    file: () => file,
    set: (line: number) => lines.setBreakpoint('every-node', file, line),
    clear: (line: number) => lines.clearBreakpoint('every-node', file, line),
    inject: (port: string, value: number) => session.graph.inject('walker', port, value),
    tick: () => {
      ticked = undefined;
      void session.tick().then((result) => {
        ticked = result;
      });
    },
    ticked: () => ticked,
    pause: () => lines.pause(),
    resume: () => lines.resume(),
    stepOver: () => lines.stepOver(),
    stepIn: () => lines.stepIn(),
    stepOut: () => lines.stepOut(),
    output: () => session.graph.readOutput('walker', 'out'),
  },
});
