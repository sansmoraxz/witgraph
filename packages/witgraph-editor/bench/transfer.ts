// How fast a Value moves between nodes, by size: a `list<u8>` through
// source → relay → relay → sink, once per run, through the loader's session
// alone and through the editor with and without previews of the bytes.
//
// Build the guests and the loader first (`cargo build -p test-components`,
// then `pnpm build:wasm` in `packages/witgraph-loader`), then:
//
//   node bench/transfer.ts [MiB...]

import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { type ComponentSource, Session } from 'witgraph-loader';
import { EditorController } from '../src/controller/editor-controller.ts';
import type { GraphJson } from '../src/controller/types.ts';

const root = new URL('../../../', import.meta.url);

/** The newest directory cargo built the guest components into. */
async function guestDir(): Promise<URL> {
  const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: fileURLToPath(root),
    encoding: 'utf8',
  });
  const build = new URL('debug/build/', pathToFileURL(`${JSON.parse(metadata).target_directory}/`));
  let newest: [number, URL] | undefined;
  for (const name of await readdir(build)) {
    if (!name.startsWith('test-components-')) continue;
    const out = new URL(`${name}/out/`, build);
    const sink = await stat(new URL('blob-sink.wasm', out)).catch(() => undefined);
    if (sink && (!newest || sink.mtimeMs > newest[0])) newest = [sink.mtimeMs, out];
  }
  if (!newest) throw new Error('build the guests: `cargo build -p test-components`');
  return newest[1];
}

const guests = await guestDir();
const wasm = await readFile(
  new URL('../../witgraph-loader/pkg/witgraph_web_bg.wasm', import.meta.url),
);
const components: ComponentSource[] = await Promise.all(
  ['blob-source', 'blob-relay', 'blob-sink'].map(async (name) => ({
    id: `test:${name}/${name}@0.1.0`,
    bytes: new Uint8Array(await readFile(new URL(`${name}.wasm`, guests))),
  })),
);
const ref = (name: string) => ({
  package: { namespace: 'test', name, version: '0.1.0' },
  world: name,
});

/** The chain, each node's bytes previewed or not. */
function graph(size: number, previews: boolean): GraphJson {
  const preview = previews ? { 'editor.preview': 'blob' } : {};
  const node = (id: string, name: string, attrs: Record<string, string> = {}) => ({
    id,
    component: ref(name),
    attrs: { ...attrs, ...(name === 'blob-sink' ? {} : preview) },
  });
  const edge = (from: string, to: string) => ({
    id: `${from}-${to}`,
    from: { node: from, port: 'blob' },
    to: { node: to, port: 'blob' },
  });
  return {
    metadata: { name: 'transfer' },
    components: ['blob-source', 'blob-relay', 'blob-sink'].map(ref),
    nodes: [
      node('source', 'blob-source', { 'inject.size': String(size) }),
      node('a', 'blob-relay'),
      node('b', 'blob-relay'),
      node('sink', 'blob-sink'),
    ],
    connections: [edge('source', 'a'), edge('a', 'b'), edge('b', 'sink')],
  };
}

/** The median of `runs` timings of `once`, after a warm-up. */
async function median(runs: number, once: (i: number) => Promise<void>): Promise<number> {
  await once(0);
  const times: number[] = [];
  for (let i = 1; i <= runs; i++) {
    const start = performance.now();
    await once(i);
    times.push(performance.now() - start);
  }
  times.sort((a, b) => a - b);
  return times[Math.floor(times.length / 2)];
}

async function session(size: number, runs: number): Promise<number> {
  const s = await Session.load({ graph: graph(size, false), components, wasm });
  try {
    s.graph.inject('source', 'size', size);
    return await median(runs, async (i) => {
      s.graph.inject('source', 'seed', i);
      await s.settle();
      if (s.graph.readOutput('sink', 'len') !== size) throw new Error('the bytes did not arrive');
    });
  } finally {
    s.dispose();
  }
}

async function editor(size: number, runs: number, previews: boolean): Promise<number> {
  const controller = new EditorController();
  await controller.load({ graph: graph(size, previews), components, wasm });
  try {
    return await median(runs, async (i) => {
      controller.injectValue('source', 'seed', String(i));
      await controller.run();
      if (controller.latestOutput('sink', 'len') !== size)
        throw new Error('the bytes did not arrive');
    });
  } finally {
    controller.dispose();
  }
}

const sizes = process.argv.slice(2).map(Number);
const MIB = 1024 * 1024;
const rows: string[][] = [['size', 'session', 'editor, no previews', 'editor, previews']];
for (const mib of sizes.length > 0 ? sizes : [1, 4, 16]) {
  const size = Math.round(mib * MIB);
  const runs = mib >= 8 ? 3 : 7;
  const ms = (t: number) => `${t.toFixed(1)} ms`;
  rows.push([
    `${mib} MiB`,
    ms(await session(size, runs)),
    ms(await editor(size, runs, false)),
    ms(await editor(size, runs, true)),
  ]);
}
const widths = rows[0].map((_, c) => Math.max(...rows.map((r) => r[c].length)));
for (const row of rows) console.log(row.map((cell, c) => cell.padEnd(widths[c])).join('  '));
