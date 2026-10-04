// Runs a witgraph graph on a JavaScript host.
//
// Each node's component is transpiled with jco and instantiated on its
// own; the witgraph scheduler (the `witgraph-web` crate, in `../pkg`)
// decides when each node's `run` is called and carries values between
// them, under the same rules as the wasmtime runtime. Every call to a
// node goes through this module, which is where debugging and simulation
// hook in: `beforeRun` and `afterRun` see (and may hold) each call,
// `onStreamItem` sees every chunk of a stream between nodes, and `mocks`
// replaces a node with a function.

import { transpile } from '@bytecodealliance/jco/component';

import initModule, {
  WebGraph,
  describeComponent as describeRaw,
  jsName as jsNameRaw,
} from '../pkg/witgraph_web.js';

/** A port of a component, as the editor catalog describes one. */
export interface Port {
  name: string;
  kind: 'value' | 'stream' | 'future';
  /** WIT-syntax rendering of the payload type, for display. */
  type_display: string;
  optional: boolean;
  docs?: string;
}

/** A component's contract, as the editor catalog describes one. */
export interface Contract {
  /** `namespace:name/world@version#<content hash>`. */
  id: string;
  package: string;
  world: string;
  content_hash?: string;
  docs?: string;
  inputs: Port[];
  outputs: Port[];
  capabilities: { name: string; implements?: string }[];
  /** Named types the ports use (from `describeComponent` only). */
  types?: { name: string; owner?: string; type_display: string; docs?: string }[];
}

/**
 * A capability of a node's contract: what the host provides for one of its
 * imports, as a wasmtime host is asked for it.
 */
export interface Capability {
  /**
   * The full interface id, version included (`demo:caps/clock@0.1.0`), or
   * the label of a labelled import (`primary`).
   */
  interface: string;
  /** For a labelled import, the full id of the interface the label stands for. */
  implements?: string;
  /** Each function and resource imported, by name, with a canonical rendering of its signature. */
  items?: Record<string, string>;
}

/** A component the graph instantiates. */
export interface ComponentSource {
  /** `namespace:name/world@version`, as the graph's component table names it. */
  id: string;
  /** The encoded component. */
  bytes: Uint8Array;
  /**
   * The WIT source text the component was built from (one document,
   * dependencies nested in it). With it, the bytes are checked against
   * their source and the graph may pin the content hashes a catalog lowered
   * from that source has; without, the contract is lowered from the bytes.
   */
  wit?: string;
}

/** The inputs or outputs of one `run`, keyed by port in `lowerCamelCase`. */
export type PortValues = Record<string, unknown>;

/** An item seen on a stream between two nodes. */
export interface StreamItem {
  /** The node whose output the stream is. */
  node: string;
  /** The output port, as the contract names it. */
  port: string;
  /** One chunk of the stream, as jco yields it. */
  item: unknown;
}

export interface SessionOptions {
  /** The graph (`witgraph_ir::Graph`), as an object or JSON text. */
  graph: object | string;
  /** Every component the graph instantiates. */
  components: ComponentSource[];
  /** The compiled `witgraph-web` module: see {@link init}. */
  wasm?: Wasm;
  /**
   * What a node imports: called for each import every time the node is
   * instantiated (when the session loads, and on every rebuild after a
   * fault, cancel or restore), with the capability of the node's contract
   * the import is, as a wasmtime host is asked for it. It returns the
   * import's implementation as jco expects it (functions by
   * `lowerCamelCase` name), or `undefined` when there is none. The
   * built-in `witgraph:runtime/host` is provided for every node.
   */
  capabilities?: (node: string, capability: Capability) => object | undefined;
  /**
   * Nodes replaced by a function of their inputs, for simulation. Only the
   * object's own properties count, and each is called as its method.
   */
  mocks?: Record<string, (inputs: PortValues) => PortValues | Promise<PortValues>>;
  /** Called before a node's `run`; the run waits for the returned promise. */
  beforeRun?: (node: string, inputs: PortValues) => void | Promise<void>;
  /** Called after a node's `run` returned; delivery waits for the promise. */
  afterRun?: (node: string, outputs: PortValues) => void | Promise<void>;
  /**
   * Called for every chunk of every stream output, as its reader takes it
   * (jco yields a stream in chunks, not item by item). What it throws is
   * reported to the console and does not reach the reader.
   */
  onStreamItem?: (item: StreamItem) => void;
}

/**
 * The compiled `witgraph-web` module (`pkg/witgraph_web_bg.wasm`), or
 * where to fetch it.
 */
export type Wasm = BufferSource | URL | WebAssembly.Module;

/** The result of a tick. */
export type TickResult =
  | { result: 'progress' | 'idle' | 'interrupted' | 'step-limit' }
  | { result: 'aborted'; node: string; message: string };

/**
 * `kebab-case` as jco names record fields and flags: `lowerCamelCase`, with
 * every word lower-cased first (`HTTP-status` is `httpStatus`). The same
 * rule as the scheduler's `jsName` (a test keeps them in step), but usable
 * before any session has loaded the module.
 */
export function camelCase(name: string): string {
  return name
    .split('-')
    .filter((word) => word.length > 0)
    .map((word, i) => {
      const lower = word.toLowerCase();
      return i === 0 ? lower : lower.charAt(0).toUpperCase() + lower.slice(1);
    })
    .join('');
}
/** Whether two `wasm` options name the same module: by content, not identity. */
function sameWasm(a: Wasm | undefined, b: Wasm | undefined): boolean {
  if (a === b) return true;
  if (a instanceof URL && b instanceof URL) return a.href === b.href;
  const bytes = (x: unknown): Uint8Array | undefined =>
    x instanceof ArrayBuffer
      ? new Uint8Array(x)
      : ArrayBuffer.isView(x)
        ? new Uint8Array(x.buffer, x.byteOffset, x.byteLength)
        : undefined;
  const [x, y] = [bytes(a), bytes(b)];
  return x !== undefined && y !== undefined && x.length === y.length && x.every((v, i) => v === y[i]);
}

/** What the built-in `fatal` throws; the scheduler turns it into a fatal fault. */
class FatalError extends Error {
  witgraphFatal = true;
}

/** The built-in `witgraph:runtime/host`. */
const host = {
  fatal: (message: string): never => {
    throw new FatalError(message);
  },
};

/** A transpiled component: instantiate it as often as it is a node. */
interface Transpiled {
  instantiate: (
    getCoreModule: (path: string) => Promise<WebAssembly.Module>,
    imports: Record<string, object>,
  ) => Promise<Record<string, any>>;
  modules: Map<string, Promise<WebAssembly.Module>>;
  /** Its imports, as jco names them. */
  imports: string[];
}

/** A node's component, and what each of its imports is (`null`: the built-in host). */
interface NodeComponent {
  transpiled: Transpiled;
  imports: [name: string, capability: Capability | null][];
}

/** The `witgraph-web` module's initialisation, the `wasm` it was given, and whether it is done. */
let initialised: { done: Promise<unknown>; wasm: Wasm | undefined; ready: boolean } | undefined;

/**
 * Initialises the `witgraph-web` module from `wasm`: the compiled module
 * (`pkg/witgraph_web_bg.wasm`) or where to fetch it. In a browser the
 * default, a URL beside the module's JavaScript, works; under Node pass the
 * file's bytes. The module is initialised once: a later call (or
 * `Session.load`, which calls this) must pass the same value or none.
 * `describeComponent` and `jsName` need it done.
 */
export async function init(wasm?: Wasm): Promise<void> {
  if (initialised === undefined) {
    const done = initModule(wasm === undefined ? undefined : { module_or_path: wasm });
    const entry = { done, wasm, ready: false };
    done.then(
      () => void (entry.ready = true),
      // A failed initialisation is not kept: a later call may try again.
      () => void (initialised === entry && (initialised = undefined)),
    );
    initialised = entry;
  } else if (wasm !== undefined && !sameWasm(wasm, initialised.wasm)) {
    throw new Error('the witgraph-web module is already initialised from another `wasm`');
  }
  await initialised.done;
}

/** Throws unless the `witgraph-web` module is initialised. */
function initialisedOrThrow(): void {
  if (!initialised?.ready) {
    throw new Error('the witgraph-web module is not initialised: call `init` (or `Session.load`) first');
  }
}

/**
 * Lowers the WIT embedded in an encoded component and describes it as the
 * editor catalog does. `id` names the component
 * (`namespace:name/world@version`). Needs {@link init}.
 */
export function describeComponent(id: string, bytes: Uint8Array): Contract {
  initialisedOrThrow();
  return describeRaw(id, bytes) as Contract;
}

/**
 * The name jco gives a WIT record field or flag in JavaScript, as the
 * scheduler has it. Needs {@link init}; {@link camelCase} does not.
 */
export function jsName(name: string): string {
  initialisedOrThrow();
  return jsNameRaw(name);
}

/** A graph loaded on this host. */
export class Session {
  /** The scheduler: inject, read, snapshot and restore go through it. */
  readonly graph: WebGraph;
  /** Every node's contract, by node id. */
  readonly contracts: Map<string, Contract>;
  #options: SessionOptions;
  #components: Map<string, NodeComponent>;
  #instances = new Map<string, Record<string, any>>();
  /**
   * Per node, how many of its generations were abandoned: a rebuild or run
   * started under an older count belongs to none.
   */
  #epoch = new Map<string, number>();
  #disposed = false;

  private constructor(graph: WebGraph, options: SessionOptions, components: Map<string, NodeComponent>) {
    this.graph = graph;
    this.#options = options;
    this.#components = components;
    // The contracts the graph was loaded with: lowered once, by `WebGraph.load`.
    this.contracts = new Map(
      [...components.keys()].map((node) => [node, graph.contract(node) as Contract]),
    );
  }

  /** Loads the graph, transpiles the components its nodes run and instantiates every node. */
  static async load(options: SessionOptions): Promise<Session> {
    await init(options.wasm);
    const graph = typeof options.graph === 'string' ? options.graph : JSON.stringify(options.graph);
    let session: Session | undefined;
    const live = (): Session => {
      if (session === undefined) throw new Error('the session is still loading');
      return session;
    };
    const web = WebGraph.load(graph, options.components, {
      run: (node: string, inputs: PortValues) => live().#run(node, inputs),
      rebuild: (node: string) => live().#instantiate(node),
      abandon: (nodes: string[]) => live().#abandon(nodes),
      close: closeUnread,
    });
    try {
      // The entry of `components` each node runs, as the graph resolved it.
      const nodes: string[] = (web.islands() as string[][]).flat();
      const entries = new Map<number, Promise<Transpiled>>();
      const transpiled = nodes.map((node) => {
        const { index, sha256 } = web.component(node) as { index: number; sha256: string };
        let entry = entries.get(index);
        if (entry === undefined) {
          entry = transpileCached(sha256, options.components[index]);
          entries.set(index, entry);
        }
        return entry;
      });
      const components = new Map<string, NodeComponent>();
      for (const [i, entry] of (await Promise.all(transpiled)).entries()) {
        const node = nodes[i];
        const capabilities = web.imports(node, entry.imports) as (Capability | null)[];
        components.set(node, {
          transpiled: entry,
          imports: entry.imports.map((name, j) => [name, capabilities[j]]),
        });
      }
      session = new Session(web, options, components);
      await Promise.all(nodes.map((node) => live().#instantiate(node)));
      return session;
    } catch (error) {
      // The graph is never handed out: its memory in the module goes now.
      if (session) session.dispose();
      else web.free();
      throw error;
    }
  }

  /**
   * Frees the graph's scheduler in the witgraph-web module and drops every
   * node instance. The session cannot be used afterwards: a tick still
   * running ends as `interrupted`, a run it holds is abandoned, and no hook
   * is called again. The scheduler holds this session's callbacks, so a
   * session that is only dropped is never collected: dispose each one (on
   * every reload of an editor's graph, say). Disposing twice does nothing.
   */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#instances.clear();
    this.graph.free();
  }

  /**
   * Runs one iteration of the graph. `stop` ends the tick early when it
   * settles. Rejects at once while another tick runs.
   */
  tick(stop?: Promise<unknown>): Promise<TickResult> {
    if (this.#disposed) return Promise.reject(new Error('the session is disposed'));
    return this.graph.tick(stop) as Promise<TickResult>;
  }

  /** Ticks until the graph is idle, at most `limit` times; returns the ticks run. */
  async settle(limit = 100): Promise<number> {
    for (let ticks = 1; ticks <= limit; ticks++) {
      const tick = await this.tick();
      if (tick.result === 'idle') return ticks;
      if (tick.result === 'aborted') throw new Error(`\`${tick.node}\`: ${tick.message}`);
    }
    throw new Error(`the graph did not settle in ${limit} ticks`);
  }

  /** Counts a generation of each node abandoned. */
  #abandon(nodes: string[]): void {
    for (const node of nodes) this.#epoch.set(node, (this.#epoch.get(node) ?? 0) + 1);
  }

  /** Whether `node` is mocked: only the mocks' own properties count. */
  #mocked(node: string): boolean {
    const mocks = this.#options.mocks;
    return mocks !== undefined && Object.hasOwn(mocks, node) && typeof mocks[node] === 'function';
  }

  /** Gives one node fresh guest state: instantiates it again, unless it is mocked. */
  async #instantiate(node: string): Promise<void> {
    if (this.#disposed) throw new Error('the session is disposed');
    if (this.#mocked(node)) return;
    const epoch = this.#epoch.get(node) ?? 0;
    const { transpiled, imports } = this.#components.get(node)!;
    const provided: Record<string, object> = {};
    for (const [name, capability] of imports) {
      const implementation =
        capability === null ? host : this.#options.capabilities?.(node, capability);
      if (!implementation) {
        const what = capability!.implements
          ? `\`${capability!.interface}\` (\`${capability!.implements}\`)`
          : `\`${capability!.interface}\``;
        throw new Error(`node \`${node}\` imports ${what}, which nothing provides`);
      }
      provided[name] = implementation;
    }
    const getCoreModule = (path: string) => transpiled.modules.get(path)!;
    const instance = await transpiled.instantiate(getCoreModule, provided);
    // An instance whose generation was abandoned meanwhile is nobody's: a
    // newer rebuild may already have run on its own.
    if (!this.#disposed && (this.#epoch.get(node) ?? 0) === epoch) this.#instances.set(node, instance);
  }

  /** One call of a node's `run`, with the hooks around it. */
  async #run(node: string, inputs: PortValues): Promise<PortValues> {
    // A run whose generation is abandoned (cancelled, say, while
    // `beforeRun` held it) stops at the next step: its node may already be
    // rebuilt, and nothing reads what it returns.
    const epoch = this.#epoch.get(node) ?? 0;
    const abandoned = () => this.#disposed || (this.#epoch.get(node) ?? 0) !== epoch;
    const options = this.#options;
    await options.beforeRun?.(node, inputs);
    if (abandoned()) throw new Error(`the run of \`${node}\` was abandoned`);
    // Hooks and mocks are called as methods of their objects.
    const outputs: PortValues =
      (this.#mocked(node)
        ? await options.mocks![node](inputs)
        : await nodeExport(this.#instances.get(node)!).run(inputs)) ?? {};
    if (abandoned()) return outputs;
    if (options.onStreamItem) {
      for (const port of this.contracts.get(node)?.outputs ?? []) {
        const key = camelCase(port.name);
        if (port.kind === 'stream' && outputs[key]) {
          const stream = outputs[key] as AsyncIterable<unknown>;
          outputs[key] = tapped(stream, (item) => {
            try {
              options.onStreamItem?.({ node, port: port.name, item });
            } catch (error) {
              console.error(`onStreamItem threw on \`${node}.${port.name}\`:`, error);
            }
          });
        }
      }
    }
    await options.afterRun?.(node, outputs);
    return outputs;
  }
}

/**
 * Drops a stream or future output nothing reads. A stream is closed, so its
 * writer's next write fails, as on wasmtime. jco's future has no way to be
 * dropped, so it is read and its value discarded: its writer's write
 * succeeds, where on wasmtime it fails.
 */
function closeUnread(value: any): void {
  if (typeof value?.[Symbol.asyncIterator] === 'function') {
    void Promise.resolve(value[Symbol.asyncIterator]().return?.()).catch(() => {});
  } else if (typeof value?.then === 'function') {
    value.then(
      () => {},
      () => {},
    );
  }
}

/** A component's `node` export: inline, or a named `…/node` interface. */
function nodeExport(instance: Record<string, any>): { run: (inputs: PortValues) => any } {
  const key = Object.keys(instance).find((k) => k === 'node' || /\/node(@|$)/.test(k));
  if (!key) throw new Error('the component exports no `node` interface');
  return instance[key];
}

/** `stream`, reporting each item as its reader takes it. */
function tapped<T>(stream: AsyncIterable<T>, seen: (item: T) => void): AsyncIterable<T> {
  return {
    [Symbol.asyncIterator]() {
      const inner = stream[Symbol.asyncIterator]();
      return {
        async next() {
          const step = await inner.next();
          if (!step.done) seen(step.value);
          return step;
        },
        // The reader stopping stops the writer.
        return: (value?: unknown) => inner.return?.(value) ?? Promise.resolve({ done: true, value }),
      } as AsyncIterator<T>;
    },
  };
}

/** Transpiled components by the SHA-256 of their bytes, kept across sessions (the latest {@link CACHED}). */
const transpileCache = new Map<string, Promise<Transpiled>>();
const CACHED = 32;

/**
 * `transpileComponent`, done once per distinct bytes (`sha256` being
 * theirs): an editor reloading its graph reuses the work.
 */
function transpileCached(sha256: string, { id, bytes }: ComponentSource): Promise<Transpiled> {
  let transpiled = transpileCache.get(sha256);
  if (transpiled === undefined) {
    transpiled = transpileComponent(id, bytes);
    // A failed transpile is not kept.
    transpiled.catch(() => transpileCache.delete(sha256));
  } else {
    transpileCache.delete(sha256);
  }
  transpileCache.set(sha256, transpiled);
  while (transpileCache.size > CACHED) {
    transpileCache.delete(transpileCache.keys().next().value!);
  }
  return transpiled;
}

async function transpileComponent(id: string, bytes: Uint8Array): Promise<Transpiled> {
  const { files, imports } = await transpile(bytes, {
    name: 'node',
    instantiation: { tag: 'async' },
    noTypescript: true,
  });
  const modules = new Map<string, Promise<WebAssembly.Module>>();
  let source: string | undefined;
  // jco makes each file's bytes itself, over an `ArrayBuffer`.
  for (const [name, contents] of files as [string, Uint8Array<ArrayBuffer>][]) {
    if (name.endsWith('.wasm')) modules.set(name, WebAssembly.compile(contents));
    else if (name.endsWith('.js')) source = new TextDecoder().decode(contents);
  }
  if (source === undefined) throw new Error(`\`${id}\` transpiled to no JavaScript`);
  const url = `data:text/javascript;base64,${base64(new TextEncoder().encode(source))}`;
  const { instantiate } = await import(/* @vite-ignore */ url);
  return { instantiate, modules, imports };
}

function base64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

export { WebGraph };
