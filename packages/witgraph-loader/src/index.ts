// Runs a witgraph graph on a JavaScript host.
//
// The graph is composed into one component with a generated hub that holds its
// values, which is transpiled with jco and instantiated once. The witgraph
// scheduler (the `witgraph-web` crate, in `../pkg`) decides when each node's `run`
// is called, under the same rules as the wasmtime runtime; values pass from node
// to node inside the composite. Every call goes through this module, which is where
// debugging and simulation hook in: `beforeRun` and `afterRun` see (and may hold)
// each call, `onStreamItem` sees every chunk of a stream between nodes, and `mocks`
// replaces a node with a function. A value reaches the host only when it asks.

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

/** A component the graph is composed of. */
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
   * What the graph imports: called for each import every time the graph is
   * instantiated (when the session loads, and on every rebuild after a
   * fault, shutdown or restore), with the capability it is, as a wasmtime host
   * is asked for it. Nodes importing one name share one import; nodes that
   * want different providers of one interface use labels. It returns the
   * import's implementation as jco expects it (functions by
   * `lowerCamelCase` name), or `undefined` when there is none. The
   * built-in `witgraph:runtime/host` is provided for every node.
   */
  capabilities?: (capability: Capability) => object | undefined;
  /**
   * Nodes replaced by a function of their inputs, for simulation: left out of
   * the composite, whose `run` of them calls this. Only the object's own
   * properties count, each is called as its method, and its inputs and outputs
   * are as jco represents them. Mocking another node composes the graph anew.
   */
  mocks?: Record<string, (inputs: PortValues) => PortValues | Promise<PortValues>>;
  /**
   * Called before a node's `run`; the run waits for the returned promise. Its
   * inputs are read with {@link Session.readInputs}, if they are wanted: nothing
   * crosses to the host unasked.
   */
  beforeRun?: (node: string) => void | Promise<void>;
  /**
   * Called after a node's `run` returned and its outputs were delivered; the
   * scheduler waits for the promise. What it returned is read with `graph.readOutput`.
   */
  afterRun?: (node: string) => void | Promise<void>;
  /**
   * Called for every chunk of every stream output something reads, as its reader
   * takes it (jco yields a stream in chunks, not item by item). What it throws is
   * reported to the console and does not reach the reader.
   */
  onStreamItem?: (item: StreamItem) => void;
  /**
   * Called with the value of every future output something reads, once it
   * resolves. Given this hook, a future is read as soon as its node's run returns,
   * rather than when its reader takes it (the reader still gets the value), so its
   * writer's write completes that much sooner. What it throws is reported to the
   * console.
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
  witgraphNode: string;
  constructor(message: string, node: string) {
    super(message);
    this.witgraphNode = node;
  }
}

/** The transpiled composite: instantiate it as often as the graph is rebuilt. */
interface Transpiled {
  instantiate: (
    getCoreModule: (path: string) => Promise<WebAssembly.Module>,
    imports: Record<string, object>,
  ) => Promise<Record<string, any>>;
  modules: Map<string, Promise<WebAssembly.Module>>;
  /** Its imports, as jco names them. */
  imports: string[];
}

/** What one of the composite's imports is. */
type ImportKind =
  | { kind: 'host'; node: string }
  | { kind: 'lend' }
  | { kind: 'mock'; node: string }
  | { kind: 'capability'; capability: Capability };

/** A node, by its position in the graph's topological order, as the hub numbers it. */
type NodeNumber = number & { readonly brand: 'node' };
/** A stream or future input, as the hub numbers it. */
type InputNumber = number & { readonly brand: 'input' };
/** An output port, as the hub numbers it. */
type OutputNumber = number & { readonly brand: 'output' };

/** How {@link instanceHooks}' code in jco's output reaches the loader, per instance. */
const INSTANCE_HOOKS = '$witgraph';
interface InstanceHooks {
  /** Each table of host objects a guest or the hub holds, by rep. */
  tables: Map<number, unknown>[];
  /** Set once the instance is abandoned: its next call of a host import fails. */
  stopped: boolean;
  /** What jco disposed, when a guest dropped it: {@link dispose} leaves it alone. */
  disposed: WeakSet<object>;
  /** A guest trapped, in whichever task: not every trap reaches a call of `run`. */
  trapped: (error: unknown) => void;
}

/** A live hub's exports, as jco names them. */
interface HubInstance {
  control: {
    run: (node: NodeNumber) => Promise<unknown>;
  } & Record<string, (...args: any[]) => any>;
  ports: Record<string, (...args: any[]) => any>;
}

/** The objects {@link dispose} has been given: each is disposed once. */
const disposed = new WeakSet<object>();

/** Disposes a handle port's object, if it can be. */
function dispose(object: unknown): void {
  const free = (object as { [Symbol.dispose]?: () => void } | undefined)?.[Symbol.dispose];
  if (typeof free !== 'function') return;
  if (disposed.has(object as object)) return;
  disposed.add(object as object);
  try {
    free.call(object);
  } catch (error) {
    console.error('disposing a handle threw:', error);
  }
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
  #transpiled: Transpiled;
  #imports: ImportKind[];
  /** The nodes, by the number the hub knows them by. */
  #nodes: string[];
  #instance: HubInstance | undefined;
  /** What the live instance's generated code told us of itself. */
  #hooks: InstanceHooks | undefined;
  /** What the hub lent the host last. */
  #lent: unknown;
  /**
   * How many instances were abandoned: a rebuild or run started under an older
   * count belongs to none.
   */
  #epoch = 0;
  /** The stream outputs of the live instance still open. */
  #streams = new Set<Tap>();
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
    transpiled: Transpiled,
    imports: ImportKind[],
    nodes: string[],
  ) {
    this.graph = watchTraps(graph, this.#trapped);
    Session.#live.add(this);
    Session.#watchUncaught();
    this.#options = options;
    this.#transpiled = transpiled;
    this.#imports = imports;
    this.#nodes = nodes;
    // The contracts the graph was loaded with: lowered once, by `WebGraph.load`.
    this.contracts = new Map(nodes.map((node) => [node, graph.contract(node) as Contract]));
    this.warnings = graph.warnings() as string[];
    for (const warning of this.warnings) console.warn(`witgraph: ${warning}`);
  }

  /** Loads the graph, composes it, transpiles the composite and instantiates it. */
  static async load(options: SessionOptions): Promise<Session> {
    await init(options.wasm);
    const graph = typeof options.graph === 'string' ? options.graph : JSON.stringify(options.graph);
    let session: Session | undefined;
    const live = (): Session => {
      if (session === undefined) throw new Error('the session is still loading');
      return session;
    };
    const mocked = Object.keys(options.mocks ?? {}).filter(
      (node) => typeof options.mocks![node] === 'function',
    );
    const known = new Set(mockableNodes(graph));
    const web = WebGraph.load(
      graph,
      options.components,
      {
        run: (node: NodeNumber) => live().#run(node),
        rebuild: () => live().#instantiate(),
        instance: () => live().#instance,
        readHandle: (output: OutputNumber) => live().#readHandle(output),
        abandon: () => live().#abandon(),
      },
      mocked.filter((node) => known.has(node)),
    );
    try {
      const nodes = web.nodes() as string[];
      const composite = web.composite() as { bytes: Uint8Array; sha256: string };
      const transpiled = await transpileCached(composite.sha256, composite.bytes);
      const imports = web.imports(transpiled.imports) as ImportKind[];
      session = new Session(web, options, transpiled, imports, nodes);
      await session.#instantiate();
      return session;
    } catch (error) {
      // The graph is never handed out: its memory in the module goes now.
      if (session) session.dispose();
      else web.free();
      throw error;
    }
  }

  /**
   * Frees the graph's scheduler in the witgraph-web module and drops its
   * instance. The session cannot be used afterwards: a tick still
   * running ends as `interrupted`, a run it holds is abandoned, and no hook
   * is called again. The scheduler holds this session's callbacks, so a
   * session that is only dropped is never collected: dispose each one (on
   * every reload of an editor's graph, say). Disposing twice does nothing.
   */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    Session.#live.delete(this);
    this.#release();
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
   * Counts the instance abandoned, and closes its streams: their writers' next
   * writes fail, so a guest's background writer stops instead of running on for
   * nobody.
   */
  #abandon(): void {
    this.#epoch++;
    this.#release();
  }

  /**
   * Lets go of the live instance. JavaScript cannot drop one, so what it does and
   * holds is ended by hand: its streams, its next call of a host import, and
   * every host object a guest or the hub still has.
   */
  #release(): void {
    this.#closeStreams();
    const hooks = this.#hooks;
    this.#instance = undefined;
    this.#hooks = undefined;
    if (!hooks) return;
    hooks.stopped = true;
    for (const table of hooks.tables) {
      for (const object of table.values()) dispose(object);
      table.clear();
    }
  }

  /** Closes the streams of the live instance that are still open. */
  #closeStreams(): void {
    const taps = this.#streams;
    this.#streams = new Set();
    for (const tap of taps) tap.close();
  }

  /** Reads the Value inputs of `node`, by `lowerCamelCase` port, as they stand now. */
  async readInputs(node: string): Promise<PortValues> {
    const inputs: PortValues = {};
    for (const port of this.contracts.get(node)?.inputs ?? []) {
      if (port.kind !== 'value') continue;
      inputs[camelCase(port.name)] = await this.graph.readInput(node, port.name);
    }
    return inputs;
  }

  /** What the hub lends the host of a handle output. */
  #readHandle(output: OutputNumber): unknown {
    this.#lent = undefined;
    this.#instance?.ports[`readHandle${output}`]();
    const lent = this.#lent;
    this.#lent = undefined;
    return lent;
  }

  /** Gives the graph fresh guest state: instantiates the composite again. */
  async #instantiate(): Promise<void> {
    if (this.#disposed) throw new Error('the session is disposed');
    const epoch = this.#epoch;
    const provided: Record<string, object> = {};
    const options = this.#options;
    for (const [i, name] of this.#transpiled.imports.entries()) {
      const what = this.#imports[i];
      switch (what.kind) {
        case 'host':
          provided[name] = {
            fatal: (message: string): never => {
              const error = new FatalError(message, what.node);
              // Known at once, whichever task of the guest called it.
              this.graph.reportTrap(error);
              throw error;
            },
          };
          break;
        case 'lend':
          provided[name] = new Proxy(
            {},
            {
              get: () => (object: unknown) => {
                this.#lent = object;
              },
            },
          );
          break;
        case 'mock':
          provided[name] = {
            run: async (inputs: PortValues) => {
              const outputs = this.graph.mockOutputs(
                what.node,
                (await options.mocks![what.node](inputs)) ?? ({} as PortValues),
              ) as PortValues;
              // A stream the host writes ends with the instance as any other.
              for (const port of this.contracts.get(what.node)?.outputs ?? []) {
                const key = camelCase(port.name);
                if (port.kind !== 'stream' || !outputs[key]) continue;
                const tap: Tap = tapped(
                  outputs[key] as AsyncIterable<unknown>,
                  () => {},
                  () => this.#streams.delete(tap),
                );
                this.#streams.add(tap);
                outputs[key] = tap;
              }
              return outputs;
            },
          };
          break;
        case 'capability': {
          const implementation = options.capabilities?.(what.capability);
          if (!implementation) {
            const { capability } = what;
            const named = capability.implements
              ? `\`${capability.interface}\` (\`${capability.implements}\`)`
              : `\`${capability.interface}\``;
            throw new Error(`the graph imports ${named}, which nothing provides`);
          }
          provided[name] = implementation;
          break;
        }
      }
    }
    const { instantiate, modules } = this.#transpiled;
    const hooks: InstanceHooks = {
      tables: [],
      stopped: false,
      disposed,
      // A call of `run` the trap ends rejects first, which names its node.
      trapped: (error) => {
        setTimeout(() => {
          if (!hooks.stopped) this.graph.reportTrap(error);
        }, 0);
      },
    };
    provided[INSTANCE_HOOKS] = hooks;
    const instance = await instantiate((path: string) => modules.get(path)!, provided);
    // An instance abandoned meanwhile is nobody's: a newer rebuild may already
    // have run on its own.
    if (!this.#disposed && this.#epoch === epoch) {
      this.#instance = instance as HubInstance;
      this.#hooks = hooks;
    } else {
      hooks.stopped = true;
    }
  }

  /** One call of the hub's `run` of a node, with the hooks around it. */
  async #run(number: NodeNumber): Promise<unknown> {
    const node = this.#nodes[number];
    // A run abandoned (shut down, say, while `beforeRun` held it) stops at the
    // next step: the graph may already be rebuilt, and nothing reads what it
    // returns.
    const epoch = this.#epoch;
    const abandoned = () => this.#disposed || this.#epoch !== epoch;
    const options = this.#options;
    await options.beforeRun?.(node);
    if (abandoned()) throw new Error(`the run of \`${node}\` was abandoned`);
    const instance = this.#instance!;
    const returned = await instance.control.run(number);
    if (abandoned()) return returned;
    this.#tap(node, instance);
    await options.afterRun?.(node);
    return returned;
  }

  /**
   * Takes every stream and future the run of `node` delivered, and gives its
   * reader what watches it, so a stream can be closed when its instance is
   * abandoned or the session disposed.
   */
  #tap(node: string, instance: HubInstance): void {
    const options = this.#options;
    const contract = this.contracts.get(node);
    const edges = this.graph.streamEdges(node) as {
      port: string;
      kind: 'stream' | 'future';
      input: InputNumber;
    }[];
    const live = () => !this.#disposed && this.#instance === instance;
    for (const { port, kind, input } of edges) {
      const value = instance.ports[`takeStream${input}`]();
      if (value === undefined) continue;
      const give = instance.ports[`giveStream${input}`];
      if (kind === 'future') {
        const future = value as PromiseLike<unknown> & Disposable;
        // jco disposes a future its reader drops unread, but its writer's
        // write still waits, on a timer that keeps the process alive. Reading
        // the future ends the write, and hands over any handle it carries.
        future[Symbol.dispose] = () => closeUnread(future);
        if (options.onFutureValue) {
          // jco's future settles once, and its reader is given the same
          // value: watching it takes nothing from the reader.
          future.then?.(
            (settled) => {
              if (!live()) return;
              try {
                options.onFutureValue?.({ node, port, value: settled });
              } catch (error) {
                console.error(`onFutureValue threw on \`${node}.${port}\`:`, error);
              }
            },
            () => {},
          );
        }
        give(future);
        continue;
      }
      const type = contract?.outputs.find((output) => output.name === port)?.type_display ?? '';
      const numeric = NUMERIC_TYPES.has(type);
      const tap: Tap = tapped(
        value as AsyncIterable<unknown>,
        (item) => {
          if (!options.onStreamItem || this.#disposed) return;
          const items =
            numeric && ArrayBuffer.isView(item) ? (item as unknown as ArrayLike<unknown>) : [item];
          try {
            options.onStreamItem({ node, port, item, items });
          } catch (error) {
            console.error(`onStreamItem threw on \`${node}.${port}\`:`, error);
          }
        },
        () => this.#streams.delete(tap),
      );
      this.#streams.add(tap);
      give(tap);
    }
  }
}

/** The ids of a graph's nodes, as its JSON has them. */
function mockableNodes(graph: string): string[] {
  const parsed = JSON.parse(graph) as { nodes?: { id: string }[] };
  return (parsed.nodes ?? []).map((node) => node.id);
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
  'readInput',
  'readOutput',
  'readHandle',
  'readOutputWave',
  'snapshot',
  'restore',
  'tick',
]);

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

/** Transpiled composites by the SHA-256 of their bytes, kept across sessions (the latest {@link CACHED}). */
const transpileCache = new Map<string, Promise<Transpiled>>();
const CACHED = 32;

/**
 * `transpileComposite`, done once per distinct bytes (`sha256` being
 * theirs): an editor reloading a graph it has seen reuses the work.
 */
function transpileCached(sha256: string, bytes: Uint8Array): Promise<Transpiled> {
  let transpiled = transpileCache.get(sha256);
  if (transpiled === undefined) {
    transpiled = transpileComposite(bytes);
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

/**
 * How a composite is transpiled. `multiMemory` makes the members' calls to the
 * hub run on the engine's own copies: without it, jco copies a list byte by byte
 * in JavaScript, with no error, about 300 times slower.
 */
export const TRANSPILE_OPTIONS = {
  name: 'graph',
  instantiation: { tag: 'async' },
  noTypescript: true,
  multiMemory: true,
} as const;

async function transpileComposite(bytes: Uint8Array): Promise<Transpiled> {
  const { files, imports } = await transpile(bytes, TRANSPILE_OPTIONS);
  const modules = new Map<string, Promise<WebAssembly.Module>>();
  let source: string | undefined;
  // jco makes each file's bytes itself, over an `ArrayBuffer`.
  for (const [name, contents] of files as [string, Uint8Array<ArrayBuffer>][]) {
    if (name.endsWith('.wasm')) modules.set(name, WebAssembly.compile(contents));
    else if (name.endsWith('.js')) source = new TextDecoder().decode(contents);
  }
  if (source === undefined) throw new Error('the composite transpiled to no JavaScript');
  source = instanceHooks(ownedFromWriter(fastListLift(source)));
  const url = `data:text/javascript;base64,${base64(new TextEncoder().encode(source))}`;
  const { instantiate } = await import(/* @vite-ignore */ url);
  return { instantiate, modules, imports };
}

/**
 * How jco lifts a resource a guest wrote to a stream or future: from the table of
 * the host's imports, where it is not, rather than the writer's, where it is. So
 * the host reading such a stream traps, with "unknown handle index".
 */
const OWNED_LIFT =
  /(_liftFlatOwn\(\{\s*componentIdx: \d+,\s*tableIdx: (\d+),\s*classNameFn: \(\) => \w+,\s*createResourceFn:\s*\(handle\) => \{\s*const rep = )(handleTable\d+)(\[\(handle << 1\) \+ 1\] % T_OWN;[\s\S]*?rscTableRemove\()(handleTable\d+)(, handle\);)/g;

/** `source`, jco's output, with {@link OWNED_LIFT} reading the writer's table. */
export function ownedFromWriter(source: string): string {
  return source.replace(
    OWNED_LIFT,
    (_, before, table, first, between, second, after) =>
      `${before}(HANDLE_TABLES[${table}] ?? ${first})${between}(HANDLE_TABLES[${table}] ?? ${second})${after}`,
  );
}

/**
 * `source`, jco's output, telling the loader what it needs to let go of an
 * instance, which JavaScript cannot drop: its tables of host objects, and a way to
 * stop it: its calls of host imports fail, so a guest's endless task ends, and its
 * scheduler stops polling for tasks that will never run, which keeps a process
 * alive, and its deadlock watchdog stops watching them.
 */
export function instanceHooks(source: string): string {
  return source
    .replace(
      /const (captureTable\d+)= new Map\(\);/g,
      `const $1= new Map(); imports['${INSTANCE_HOOKS}']?.tables.push($1);`,
    )
    .replaceAll(
      'if (rsc[symbolDispose]) rsc[symbolDispose]();',
      `imports['${INSTANCE_HOOKS}']?.disposed.add(rsc); if (rsc[symbolDispose]) rsc[symbolDispose]();`,
    )
    .replace(
      'err = _normalizeCoreTrap(err);',
      `err = _normalizeCoreTrap(err); imports['${INSTANCE_HOOKS}']?.trapped(err);`,
    )
    .replace(
      'while (result !== ComponentAsyncState.TickResult.DONE) {',
      `while (result !== ComponentAsyncState.TickResult.DONE && !imports['${INSTANCE_HOOKS}']?.stopped) {`,
    )
    .replace(
      'if (STORE_TRAP.error !== null || STORE_ASYNC_STATE.pendingHostOperations > 0) { return; }',
      `if (imports['${INSTANCE_HOOKS}']?.stopped || STORE_TRAP.error !== null || STORE_ASYNC_STATE.pendingHostOperations > 0) { return; }`,
    )
    .replace(
      "_debugLog('[_lowerImport()] calling lowered import', { importFn, params });",
      `if (imports['${INSTANCE_HOOKS}']?.stopped) { throw new Error('the instance was abandoned'); }`,
    );
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
