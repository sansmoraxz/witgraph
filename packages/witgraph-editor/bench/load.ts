// How long an edit of a graph takes to reach a running one: generating the hub,
// composing it, transpiling the composite with jco and instantiating it, by the
// number of nodes in a chain. A new number of nodes is a new composite, so nothing
// is served from the loader's cache.
//
// Build the guests and the loader first (`cargo build -p test-components`, then
// `pnpm build:wasm` in `packages/witgraph-loader`), then:
//
//   node bench/load.ts [nodes...]

import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { type ComponentSource, nodeDescription, Session } from 'witgraph-loader';

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
const ref = (name: string) => ({ component: name, world: `test:${name}/${name}@0.1.0` });
const components: ComponentSource[] = await Promise.all(
  ['blob-source', 'blob-relay', 'blob-sink'].map(async (name) => {
    const read = async (file: string) => new Uint8Array(await readFile(new URL(file, guests)));
    return {
      name,
      description: nodeDescription(
        JSON.parse(await readFile(new URL(`${name}.node.json`, guests), 'utf8')),
      ),
      wit: await read(`${name}.wit.wasm`),
      bytes: await read(`${name}.wasm`),
    };
  }),
);

const sizes = process.argv.slice(2).map(Number);
console.log('nodes  load');
for (const nodes of sizes.length > 0 ? sizes : [4, 16, 32]) {
  const kind = (i: number) =>
    ref(i === 0 ? 'blob-source' : i === nodes - 1 ? 'blob-sink' : 'blob-relay');
  const graph = {
    metadata: { name: `chain-${nodes}` },
    components: Object.fromEntries(
      ['blob-source', 'blob-relay', 'blob-sink'].map((name) => [name, name]),
    ),
    nodes: Array.from({ length: nodes }, (_, i) => ({ id: `n${i}`, ...kind(i) })),
    connections: Array.from({ length: nodes - 1 }, (_, i) => ({
      id: `c${i}`,
      from: { node: `n${i}`, port: 'blob' },
      to: { node: `n${i + 1}`, port: 'blob' },
    })),
  };
  const start = performance.now();
  const session = await Session.load({ graph, components, wasm });
  console.log(`${String(nodes).padEnd(6)} ${(performance.now() - start).toFixed(0)} ms`);
  session.dispose();
}
