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
  describeComponent as describeRaw,
  jsName as jsNameRaw,
  WebGraph,
} from '../pkg/witgraph_web.js';

/** A port of a component, as the editor catalog describes one. */
export interface Port {
  name: string;
  kind: 'value' | 'stream' | 'future' | 'handle';
  /** WIT-syntax rendering of the payload type, for display. */
  type_display: string;
  /**
   * Handle ports only: the resource the port carries, which a capability
   * provides. An input borrows it for its `run`, an output hands it to the
   * host, which disposes it (`[Symbol.dispose]()`) once nothing holds it.
   */
  handle?: { interface: string; resource: string };
  optional: boolean;
  docs?: string;
  /**
   * Output ports only: how an editor should preview the port (`image`,
   * say), as the component's `witgraph-viewer` custom section declares it.
   */
  viewer?: string;
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
  /**
   * What was wrong with the component but did not stop it loading (from
   * `describeComponent` only): a viewer hint naming an output it does not
   * have, say.
   */
  warnings?: string[];
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
  /**
   * The chunk's items: a stream of numbers (see {@link NUMERIC_TYPES})
   * comes in typed arrays, which are its items as they are, not copied;
   * any other chunk is one item (a record, or a `list<u8>`'s `Uint8Array`).
   */
  items: ArrayLike<unknown>;
}

/** The value a future between two nodes resolved to. */
export interface FutureItem {
  /** The node whose output the future is. */
  node: string;
  /** The output port, as the contract names it. */
  port: string;
  /** The value, as jco represents it. */
  value: unknown;
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
  /**
   * Called with the value of every future output once it resolves. Given
   * this hook, a future is read as soon as its node's run returns, rather
   * than when its reader takes it (the reader still gets the value), so its
   * writer's write completes that much sooner. What it throws is reported
   * to the console.
   */
  onFutureValue?: (item: FutureItem) => void;
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
  return (
    x !== undefined && y !== undefined && x.length === y.length && x.every((v, i) => v === y[i])
  );
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

/** Disposes a handle port's object, if it can be. */
function dispose(object: unknown): void {
  const free = (object as { [Symbol.dispose]?: () => void } | undefined)?.[Symbol.dispose];
  if (typeof free !== 'function') return;
  try {
    free.call(object);
  } catch (error) {
    console.error('disposing a handle threw:', error);
  }
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
      () => {
        entry.ready = true;
      },
      () => {
        // A failed initialisation is not kept: a later call may try again.
        if (initialised === entry) initialised = undefined;
      },
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
    throw new Error(
      'the witgraph-web module is not initialised: call `init` (or `Session.load`) first',
    );
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
  /**
   * What was wrong with the components but did not stop the load (a viewer
   * hint naming an output the component does not have, say). Each is also
   * reported to the console as the session loads.
   */
  readonly warnings: readonly string[];
  #options: SessionOptions;
  #components: Map<string, NodeComponent>;
  #instances = new Map<string, Record<string, any>>();
  /**
   * Per node, how many of its generations were abandoned: a rebuild or run
   * started under an older count belongs to none.
   */
  #epoch = new Map<string, number>();
  /** Per node, the stream outputs of its runs still open. */
  #streams = new Map<string, Set<Tap>>();
  #disposed = false;
  /** Set once the scheduler's module trapped: see {@link broken}. */
  #trapped: { broken?: Error } = {};
  /** What fails each tick still running, should the scheduler trap. */
  #ticking = new Set<(error: Error) => void>();
  /** Every session not yet disposed: a trap breaks them all. */
  static #live = new Set<Session>();
  static #watching = false;

  private constructor(
    graph: WebGraph,
    options: SessionOptions,
    components: Map<string, NodeComponent>,
  ) {
    this.graph = watchTraps(graph, this.#trapped);
    Session.#live.add(this);
    Session.#watchUncaught();
    this.#options = options;
    this.#components = components;
    // The contracts the graph was loaded with: lowered once, by `WebGraph.load`.
    this.contracts = new Map(
      [...components.keys()].map((node) => [node, graph.contract(node) as Contract]),
    );
    this.warnings = graph.warnings() as string[];
    for (const warning of this.warnings) console.warn(`witgraph: ${warning}`);
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
      // The component each node runs, as the graph resolved it: its entry of
      // `components`, or, for a node whose links apply, that entry composed
      // with its providers (`bytes`). Nodes running the same bytes share a
      // transpile.
      const nodes: string[] = (web.islands() as string[][]).flat();
      const entries = new Map<string, Promise<Transpiled>>();
      const transpiled = nodes.map((node) => {
        const { index, sha256, bytes } = web.component(node) as {
          index: number;
          sha256: string;
          bytes?: Uint8Array;
        };
        let entry = entries.get(sha256);
        if (entry === undefined) {
          const { id } = options.components[index];
          entry = transpileCached(sha256, bytes ? { id, bytes } : options.components[index]);
          entries.set(sha256, entry);
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
    Session.#live.delete(this);
    for (const node of [...this.#streams.keys()]) this.#close(node);
    this.#instances.clear();
    // A graph the scheduler trapped in is still held by the call that
    // trapped: freeing it throws. Its memory stays with the module.
    if (!this.#trapped.broken) this.graph.free();
  }

  /**
   * Why the session can no longer be used, once the scheduler's
   * WebAssembly module has trapped (run out of memory, say): an error
   * naming the call it trapped in. Every call to {@link graph} throws it
   * from then on. Load the graph again in a new session.
   */
  get broken(): Error | undefined {
    return this.#trapped.broken;
  }

  /**
   * Runs one iteration of the graph. `stop` ends the tick early when it
   * settles. Rejects at once while another tick runs.
   */
  tick(stop?: Promise<unknown>): Promise<TickResult> {
    if (this.#disposed) return Promise.reject(new Error('the session is disposed'));
    return new Promise<TickResult>((resolve, reject) => {
      // Should the scheduler trap while the tick runs, the tick's own
      // promise never settles: this rejects it instead.
      this.#ticking.add(reject);
      (this.graph.tick(stop) as Promise<TickResult>)
        .then(resolve, reject)
        .finally(() => this.#ticking.delete(reject));
    });
  }

  /** Breaks the session (see {@link broken}) and fails its ticks still running. */
  #break(error: Error): void {
    this.#trapped.broken ??= error;
    for (const fail of this.#ticking) fail(this.#trapped.broken);
    this.#ticking.clear();
  }

  /**
   * Watches for a trap of the scheduler's module that no call sees: one in
   * the work of a tick, which the module runs from microtasks, so it is
   * thrown into the host (the browser's `error` event; Node's
   * `uncaughtExceptionMonitor`, which leaves Node's own handling as it is)
   * and the tick never settles. Every live session shares the module, so
   * every one is broken. A guest's trap is not the scheduler's: it is the
   * fault of its node.
   */
  static #watchUncaught(): void {
    if (Session.#watching) return;
    Session.#watching = true;
    const seen = (error: unknown) => {
      if (!(error instanceof WebAssembly.RuntimeError)) return;
      if (!/witgraph_web/.test(error.stack ?? '')) return;
      const broken = new Error(
        `the scheduler stopped while running a tick (${error.message}): ` +
          'the session can no longer be used; load the graph again',
        { cause: error },
      );
      for (const session of Session.#live) session.#break(broken);
    };
    const host = globalThis as {
      addEventListener?: (type: 'error', listener: (event: { error?: unknown }) => void) => void;
      process?: {
        on?: (event: 'uncaughtExceptionMonitor', listener: (error: unknown) => void) => void;
      };
    };
    host.addEventListener?.('error', (event) => seen(event.error));
    host.process?.on?.('uncaughtExceptionMonitor', seen);
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

  /**
   * Counts a generation of each node abandoned, and closes the streams its
   * runs returned: their writers' next writes fail, so a guest's background
   * writer stops instead of running on for nobody.
   */
  #abandon(nodes: string[]): void {
    for (const node of nodes) {
      this.#epoch.set(node, (this.#epoch.get(node) ?? 0) + 1);
      this.#close(node);
    }
  }

  /** Closes the streams `node`'s runs returned that are still open. */
  #close(node: string): void {
    const taps = this.#streams.get(node);
    this.#streams.delete(node);
    for (const tap of taps ?? []) tap.close();
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
    if (!this.#disposed && (this.#epoch.get(node) ?? 0) === epoch)
      this.#instances.set(node, instance);
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
    if (abandoned()) {
      // Nothing reads what an abandoned run returns: its streams' writers
      // stop now, rather than run on for nobody.
      for (const port of this.contracts.get(node)?.outputs ?? []) {
        if (port.kind === 'stream' || port.kind === 'future') {
          closeUnread(outputs[camelCase(port.name)]);
        }
        // Nothing will hold it, so nothing else would dispose it.
        if (port.kind === 'handle') dispose(outputs[camelCase(port.name)]);
      }
      return outputs;
    }
    // Every stream output is tapped, so it can be closed when its
    // generation is abandoned or the session disposed.
    for (const port of this.contracts.get(node)?.outputs ?? []) {
      const key = camelCase(port.name);
      if (port.kind === 'future') {
        // jco disposes a future its reader drops unread, but its writer's
        // write still waits, on a timer that keeps the process alive. Reading
        // the future ends the write, and hands over any handle it carries.
        const future = outputs[key] as (PromiseLike<unknown> & Disposable) | undefined;
        if (future) future[Symbol.dispose] = () => closeUnread(future);
      }
      if (port.kind === 'future' && options.onFutureValue) {
        // jco's future settles once, and its reader is given the same
        // value: watching it takes nothing from the reader.
        const future = outputs[key] as PromiseLike<unknown> | undefined;
        future?.then?.(
          (value) => {
            if (this.#disposed || abandoned()) return;
            try {
              options.onFutureValue?.({ node, port: port.name, value });
            } catch (error) {
              console.error(`onFutureValue threw on \`${node}.${port.name}\`:`, error);
            }
          },
          () => {},
        );
      }
      if (port.kind !== 'stream' || !outputs[key]) continue;
      const numeric = NUMERIC_TYPES.has(port.type_display);
      const open = this.#streams.get(node) ?? new Set<Tap>();
      this.#streams.set(node, open);
      const tap: Tap = tapped(
        outputs[key] as AsyncIterable<unknown>,
        (item) => {
          if (!options.onStreamItem || this.#disposed) return;
          const items =
            numeric && ArrayBuffer.isView(item) ? (item as unknown as ArrayLike<unknown>) : [item];
          try {
            options.onStreamItem({ node, port: port.name, item, items });
          } catch (error) {
            console.error(`onStreamItem threw on \`${node}.${port.name}\`:`, error);
          }
        },
        () => open.delete(tap),
      );
      open.add(tap);
      outputs[key] = tap;
    }
    await options.afterRun?.(node, outputs);
    return outputs;
  }
}

/**
 * Drops a stream or future output nothing reads, or a future its reader
 * dropped. A stream is closed, so its writer's next write fails, as on
 * wasmtime. jco's future has no way to be dropped, so it is read and its value
 * discarded: its writer's write succeeds, where on wasmtime it fails.
 */
function closeUnread(value: any): void {
  if (typeof value?.[Symbol.asyncIterator] === 'function') {
    void Promise.resolve(value[Symbol.asyncIterator]().return?.()).catch(() => {});
  } else if (typeof value?.then === 'function') {
    // A handle it carried is nobody else's now.
    value.then(dispose, () => {});
  }
}

/**
 * `graph`, every method call of which is watched for a trap of the
 * scheduler's WebAssembly module (`WebAssembly.RuntimeError`: out of
 * memory, say). A trap stops the scheduler mid-call, skipping the code
 * that would let go of what the call held, the graph among it: every later
 * call would fail on that. So the first trap is recorded in `state` as an
 * error naming the call, and it is thrown for the trap and for every call
 * after. Other errors pass through as they are.
 */
export function watchTraps<T extends object>(graph: T, state: { broken?: Error }): T {
  const show = (arg: unknown) =>
    typeof arg === 'string'
      ? JSON.stringify(arg)
      : typeof arg === 'number'
        ? String(arg)
        : typeof arg;
  return new Proxy(graph, {
    get(target, key) {
      const member: unknown = Reflect.get(target, key, target);
      if (typeof member !== 'function') return member;
      return (...args: unknown[]) => {
        // What returns a promise never throws: it rejects.
        const fail = (error: unknown) => {
          if (ASYNC_CALLS.has(String(key))) return Promise.reject(error);
          throw error;
        };
        if (state.broken) return fail(state.broken);
        const seen = (error: unknown) => {
          if (!(error instanceof WebAssembly.RuntimeError)) return error;
          state.broken = new Error(
            `the scheduler stopped in \`${String(key)}(${args.map(show).join(', ')})\` ` +
              `(${error.message}): the session can no longer be used; load the graph again`,
            { cause: error },
          );
          return state.broken;
        };
        try {
          const result: unknown = member.apply(target, args);
          if (result instanceof Promise) {
            return result.catch((error: unknown) => {
              throw seen(error);
            });
          }
          return result;
        } catch (error) {
          return fail(seen(error));
        }
      };
    },
  });
}

/** The scheduler's calls that return a promise. */
const ASYNC_CALLS = new Set([
  'inject',
  'injectWave',
  'clearInput',
  'readOutput',
  'readHandle',
  'readOutputWave',
  'snapshot',
  'restore',
  'tick',
]);

/** A component's `node` export: inline, or a named `…/node` interface. */
function nodeExport(instance: Record<string, any>): { run: (inputs: PortValues) => any } {
  const key = Object.keys(instance).find((k) => k === 'node' || /\/node(@|$)/.test(k));
  if (!key) throw new Error('the component exports no `node` interface');
  return instance[key];
}

/** The number types: jco hands over a stream of one of them in typed arrays. */
export const NUMERIC_TYPES: ReadonlySet<string> = new Set([
  'u8',
  'u16',
  'u32',
  'u64',
  's8',
  's16',
  's32',
  's64',
  'f32',
  'f64',
]);

/** A stream output as its reader sees it: see {@link tapped}. */
interface Tap extends AsyncIterable<unknown> {
  /** Ends the stream for its reader and stops its writer. */
  close(): void;
}

/**
 * `stream`, reporting each chunk as its reader takes it. Closing it, or
 * the stream ending, calls `ended` once; a closed stream reads as ended.
 */
function tapped(
  stream: AsyncIterable<unknown>,
  seen: (chunk: unknown) => void,
  ended: () => void,
): Tap {
  let inner: AsyncIterator<unknown> | undefined;
  let closed = false;
  const done = { done: true, value: undefined } as const;
  const end = () => {
    if (closed) return;
    closed = true;
    ended();
  };
  // Stopping the reader stops the writer: its next write fails.
  const stop = async () => {
    end();
    const stopped = inner ?? stream[Symbol.asyncIterator]();
    inner = undefined;
    await Promise.resolve(stopped.return?.()).catch(() => {});
    return done;
  };
  return {
    close: () => void stop(),
    [Symbol.asyncIterator]() {
      if (!closed) inner ??= stream[Symbol.asyncIterator]();
      return {
        async next() {
          if (closed || !inner) return done;
          let step: IteratorResult<unknown>;
          try {
            step = await inner.next();
          } catch (error) {
            // A read cut short by closing the stream is its end.
            if (closed) return done;
            throw error;
          }
          if (closed) {
            // Read as it closed, so no reader takes it: a handle is ours to drop.
            if (!step.done) dispose(step.value);
            return done;
          }
          if (step.done) end();
          else seen(step.value);
          return step;
        },
        return: stop,
      };
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
  source = fastListLift(source);
  const url = `data:text/javascript;base64,${base64(new TextEncoder().encode(source))}`;
  const { instantiate } = await import(/* @vite-ignore */ url);
  return { instantiate, modules, imports };
}

/**
 * The loop with which jco lifts every element of a list a guest returns,
 * one call (and one `DataView`) per element: a `list<u8>` of a few
 * megabytes takes seconds. A list of numbers is its elements' bytes as
 * they are in linear memory, so it is lifted here in one copy instead, with
 * the same effect on jco's lifting context; anything else (a list too long
 * for the storage left, which jco reports) goes through jco's loop.
 */
const LIST_LOOP = `        ctx.storagePtr = dataPtr;
        const val = [];
        for (var i = 0; i < len; i++) {`;
const LIST_COPY = `        if (
          typedArray !== undefined &&
          elemSize32 === typedArray.BYTES_PER_ELEMENT &&
          (ctx.storageLen === undefined || ctx.storageLen >= len * elemSize32)
        ) {
          const end = dataPtr + len * elemSize32;
          const copied = new typedArray(ctx.memory.buffer.slice(dataPtr, end));
          ctx.storagePtr = originalPtr !== null ? originalPtr : end;
          if (originalLen !== null) ctx.storageLen = originalLen;
          else if (ctx.storageLen !== undefined) ctx.storageLen -= len * elemSize32;
          return [copied, ctx];
        }
`;

/** `source`, jco's output, with {@link LIST_COPY} ahead of its list loop, if it has one. */
export function fastListLift(source: string): string {
  return source.includes(LIST_LOOP) ? source.replace(LIST_LOOP, LIST_COPY + LIST_LOOP) : source;
}

function base64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

export { WebGraph };
