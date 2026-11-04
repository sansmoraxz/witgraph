// Runs `test-fixtures/scenarios` on JavaScript. `witgraph-runtime`'s
// `tests/scenarios.rs` runs the same files on wasmtime, so the two hosts answer to
// one set of expectations. Build the guests first: `cargo build -p test-components`.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdir, readFile, stat } from 'node:fs/promises';
import { test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { type ComponentSource, Session } from '../src/index.ts';

const root = new URL('../../../', import.meta.url);

/** The directory cargo built the guest components into. */
async function guestDir(): Promise<URL> {
  const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
    cwd: fileURLToPath(root),
    encoding: 'utf8',
  });
  const target = pathToFileURL(`${JSON.parse(metadata).target_directory}/`);
  const build = new URL('debug/build/', target);
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
const directory = new URL('test-fixtures/scenarios/', root);

interface Step {
  do: string;
  at?: string;
  node?: string;
  wave?: string;
  error?: string;
  // biome-ignore lint/suspicious/noExplicitAny: each step's own shape
  expect?: any;
  snapshot?: unknown;
  patch?: Record<string, unknown>;
}

interface Scenario {
  nodes: { id: string; guest: string }[];
  connections: { id: string; from: string; to: string; feedback?: boolean }[];
  steps: Step[];
}

function componentRef(guest: string) {
  return { package: { namespace: 'test', name: guest, version: '0.1.0' }, world: guest };
}

function port(text: string) {
  const [node, name] = text.split('.');
  return { node, port: name };
}

async function load(scenario: Scenario): Promise<Session> {
  const used = [...new Set(scenario.nodes.map((node) => node.guest))];
  const components: ComponentSource[] = await Promise.all(
    used.map(async (guest) => ({
      id: `test:${guest}/${guest}@0.1.0`,
      bytes: new Uint8Array(await readFile(new URL(`${guest}.wasm`, guests))),
    })),
  );
  const graph = {
    metadata: { name: 'scenario' },
    components: used.map(componentRef),
    nodes: scenario.nodes.map(({ id, guest }) => ({ id, component: componentRef(guest) })),
    connections: scenario.connections.map(({ id, from, to, feedback }) => ({
      id,
      from: port(from),
      to: port(to),
      ...(feedback ? { feedback: true } : {}),
    })),
  };
  return Session.load({ graph, components, wasm });
}

/** `action` against what `step` says of it: an error holding its `error`, or none. */
async function outcome(step: Step, action: () => unknown): Promise<void> {
  const where = JSON.stringify(step);
  try {
    await action();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    assert.ok(step.error !== undefined, `${where}: ${message}`);
    assert.ok(message.includes(step.error), `${where}: ${message}`);
    return;
  }
  assert.equal(step.error, undefined, `${where}: no error, wanted one`);
}

async function run(scenario: Scenario): Promise<void> {
  const session = await load(scenario);
  const graph = session.graph;
  let taken: Record<string, unknown> | undefined;
  try {
    for (const step of scenario.steps) {
      const where = JSON.stringify(step);
      const [node, name] = (step.at ?? '').split('.');
      switch (step.do) {
        case 'inject':
          await outcome(step, () => graph.injectWave(node, name, step.wave as string));
          break;
        case 'clear':
          await outcome(step, () => graph.clearInput(node, name));
          break;
        case 'rerun':
          await outcome(step, () => graph.rerun(step.node as string));
          break;
        case 'shutdown':
          graph.shutdown();
          break;
        case 'tick': {
          const tick = await session.tick();
          if (step.expect) assert.equal(tick.result, step.expect, where);
          break;
        }
        case 'settle': {
          let settled = false;
          for (let i = 0; i < 100 && !settled; i++) {
            const tick = await session.tick();
            if (tick.result === 'idle') settled = true;
            else assert.equal(tick.result, 'progress', where);
          }
          assert.ok(settled, `${where}: never settled`);
          break;
        }
        case 'output': {
          const read = await graph.readOutputWave(node, name);
          assert.equal(read ?? null, step.expect, where);
          break;
        }
        case 'runs': {
          const runs: Record<string, number> = {};
          for (const event of graph.takeTrace() as { event: string; node?: string }[]) {
            if (event.event === 'run-started') {
              const of = event.node as string;
              runs[of] = (runs[of] ?? 0) + 1;
            }
          }
          for (const [of, count] of Object.entries(step.expect)) {
            assert.equal(runs[of] ?? 0, count, `${where}: runs of \`${of}\``);
          }
          const unlisted = Object.keys(runs).filter((of) => !(of in step.expect));
          assert.deepEqual(unlisted, [], `${where}: ran these too`);
          break;
        }
        case 'faults': {
          const faults = graph.takeFaults() as { culprit?: string; fault: { kind: string } }[];
          const wanted = step.expect as (string | { kind: string; culprit?: string })[];
          assert.equal(faults.length, wanted.length, `${where}: ${JSON.stringify(faults)}`);
          wanted.forEach((want, at) => {
            const kind = typeof want === 'string' ? want : want.kind;
            assert.equal(faults[at].fault.kind, kind, where);
            if (typeof want !== 'string' && want.culprit) {
              assert.equal(faults[at].culprit, want.culprit, where);
            }
          });
          break;
        }
        case 'phase':
          assert.equal(graph.nodeState(step.node as string).phase, step.expect, where);
          break;
        case 'snapshot': {
          taken = JSON.parse((await graph.snapshot()) as string) as Record<string, unknown>;
          for (const [field, want] of Object.entries(step.expect)) {
            assert.deepEqual(taken[field], want, `${where}: \`${field}\``);
          }
          break;
        }
        case 'restore': {
          const snapshot = { ...((step.snapshot ?? taken) as object), ...step.patch };
          await outcome(step, () => graph.restore(JSON.stringify(snapshot)));
          break;
        }
        default:
          assert.fail(`no step \`${step.do}\``);
      }
    }
  } finally {
    session.dispose();
  }
}

for (const file of (await readdir(directory)).filter((f) => f.endsWith('.json')).sort()) {
  test(`scenario: ${file}`, async () => {
    await run(JSON.parse(await readFile(new URL(file, directory), 'utf8')) as Scenario);
  });
}
