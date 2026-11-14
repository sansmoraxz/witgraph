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

import { $init, generate } from '@bytecodealliance/jco-transpile/component';
import initModule, {
  describeComponent as describeRaw,
  encodeWit as encodeWitRaw,
  GraphChecker,
  instrumentModule,
  jsName as jsNameRaw,
  type ModuleVariables,
  moduleVariables,
  WebGraph,
} from '../pkg/witgraph_web.js';
import { ownLines, type SessionLines, type SourceProbe, type SourceVariable } from './lines.ts';

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
   * say), as the component's node description declares it.
   */
  viewer?: string;
}

/**
 * A world, as a component provides it: the contract a node of it runs, as the
 * editor catalog describes one.
 */
export interface Contract {
  /** `component/namespace:name/world@version`. */
  id: string;
  /** The component, by its name in the graph's component table. */
  component: string;
  /** The world, `namespace:name/world@version`. */
  world: string;
  /** The world's WIT package, `namespace:name@version`. */
  package: string;
  /** The world's doc comment. */
  docs?: string;
  inputs: Port[];
  outputs: Port[];
  capabilities: { name: string; implements?: string }[];
  /** Named types the ports use (from `describeComponent` only). */
  types?: { name: string; owner?: string; type_display: string; docs?: string }[];
  /**
   * What was wrong with the component but did not stop it loading (from
   * `describeComponent` only): a viewer naming an output it does not have,
   * say.
   */
  warnings?: string[];
}

/**
 * What a component is: the WIT package its worlds are in, the worlds it
 * provides, and how an editor previews their outputs. Where its WIT and bytes
 * are is the host's business.
 */
export interface NodeDescription {
  /** The WIT package its worlds are in, `namespace:name@version`. */
  wit: string;
  /** Every world of `wit` it provides, `namespace:name/world@version`. */
  worlds: string[];
  /** How an editor previews outputs: per world, output port to viewer. */
  viewers?: Record<string, Record<string, string>>;
}

/**
 * The description in a node manifest (`*.node.json`), without where its
 * files are.
 */
export function nodeDescription(manifest: NodeDescription): NodeDescription {
  const { wit, worlds, viewers } = manifest;
  return viewers ? { wit, worlds, viewers } : { wit, worlds };
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

/**
 * A component the graph is composed of. What it is comes from its
 * description and WIT alone; its bytes are checked to target the worlds nodes run of it.
 */
export interface ComponentSource {
  /** Its name in the graph's component table. */
  name: string;
  /** What it is. */
  description: NodeDescription;
  /** Its WIT, as one wasm-encoded package (`wasm-tools component wit --wasm`). */
  wit: Uint8Array;
  /** The encoded component: what runs. Not needed to describe or check a graph. */
  bytes?: Uint8Array;
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
  /**
   * Puts a probe on each source line of the guests with debug info, and calls this at those
   * whose flags are set. A debugger is {@link RemoteSession.lines}: it blocks in a probe, which
   * only a worker can.
   */
  lines?: SessionLines;
}

/**
 * The compiled `witgraph-web` module (`pkg/witgraph_web_bg.wasm`), or
 * where to fetch it.
 */
export type Wasm = BufferSource | URL | WebAssembly.Module;

/** The result of a tick. */
export type TickResult =
  | { result: 'progress' | 'idle' | 'interrupted' | 'step-limit' }
  | { result: 'faulted'; node?: string; fault: { kind: string; message: string } };

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
    instantiateCore:
      | ((
          module: WebAssembly.Module,
          imports: WebAssembly.Imports,
        ) => Promise<WebAssembly.Instance>)
      | undefined,
    options: { onTrap: (error: unknown) => void },
  ) => Promise<Record<string, any>>;
  modules: Map<string, Promise<WebAssembly.Module>>;
  /** Its imports, as jco names them. */
  imports: string[];
  /** The core modules with probes in them. */
  probed: {
    module: WebAssembly.Module;
    probes: SourceProbe[];
    calls: Uint32Array;
    sha256: string;
    variables?: ModuleVariables;
  }[];
}

/** What `instrumentModule` makes of a core module. */
interface InstrumentedModule {
  bytes: Uint8Array;
  sha256: string;
  probes: SourceProbe[];
  calls: Uint32Array;
  spill: { base: number; globals: number };
}

/** What one of the composite's imports is. */
type ImportKind =
  | { kind: 'host'; node: string }
  | { kind: 'lend' }
  | { kind: 'mock'; node: string }
  | { kind: 'capability'; capability: Capability };

/** A node, by its position in the graph's topological order, as the hub numbers it. */
type NodeNumber = number & { readonly brand: 'node' };

/** A live hub's exports, as jco names them. */
interface HubInstance {
  _util: {
    stop: () => void;
    whenIdle: (groups?: object[]) => Promise<void>;
    lastTaskGroup: () => object | undefined;
  };
  control: {
    run: (node: NodeNumber) => Promise<unknown>;
  } & Record<string, (...args: any[]) => any>;
  ports: Record<string, (...args: any[]) => any>;
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
 * Describes a component as the editor catalog does, from its description and WIT
 * (no bytes): one contract per node world it provides, its named types and
 * warnings included. `name` is its name in the graph's component table. Needs
 * {@link init}.
 */
export function describeComponent(
  name: string,
  description: NodeDescription,
  wit: Uint8Array,
): Contract[] {
  initialisedOrThrow();
  return describeRaw(name, description, wit) as Contract[];
}

/**
 * WIT text (one document, the packages it uses nested in it) as one
 * wasm-encoded package: what a {@link ComponentSource}'s `wit` is. `name` is
 * its pseudo-path in errors. Needs {@link init}.
 */
export function encodeWit(name: string, wit: string): Uint8Array {
  initialisedOrThrow();
  return encodeWitRaw(name, wit);
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
  /** What compiling warned of the graph: it runs, but look at these. */
  readonly diagnostics: readonly Diagnostic[];
  #options: SessionOptions;
  #transpiled: Transpiled;
  #imports: ImportKind[];
  /** The nodes, by the number the hub knows them by. */
  #nodes: string[];
  #instance: HubInstance | undefined;
  /** By node: its last run in this instance, to tell its guest work from the rest's. */
  #taskGroups = new Map<NodeNumber, object>();
  /** What the hub lent the host last. */
  #lent: unknown;
  /**
   * How many instances were abandoned: a rebuild or run started under an older
   * count belongs to none.
   */
  #epoch = 0;
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
    this.diagnostics = graph.diagnostics() as Diagnostic[];
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
        readHandle: (name: string) => live().#readHandle(name),
        whenIdle: (nodes?: NodeNumber[]) => live().#whenIdle(nodes),
        abandon: () => live().#abandon(),
      },
      mocked.filter((node) => known.has(node)),
      options.lines !== undefined,
    );
    try {
      const nodes = web.nodes() as string[];
      const composite = web.composite() as { bytes: Uint8Array; sha256: string };
      const transpiled = await transpileCached(composite.sha256, composite.bytes, options.lines);
      const imports = web.imports(transpiled.imports) as ImportKind[];
      const ofModule = web.moduleNodes() as Record<string, string[]>;
      options.lines?.modules?.(
        transpiled.probed.map(({ probes, calls, sha256, variables }) => ({
          probes,
          calls,
          nodes: ofModule[sha256] ?? [],
          variablesAt: (offset, flags, memory) =>
            (variables?.at(offset, flags, memory) ?? []) as SourceVariable[],
        })),
      );
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
      if (tick.result === 'faulted') {
        const at = tick.node ? `\`${tick.node}\`: ` : '';
        throw new Error(`${at}${tick.fault.message}`);
      }
    }
    throw new Error(`the graph did not settle in ${limit} ticks`);
  }

  /** Counts the instance abandoned, and ends it. */
  #abandon(): void {
    this.#epoch++;
    this.#release();
  }

  /** Ends the live instance. */
  #release(): void {
    const instance = this.#instance;
    this.#instance = undefined;
    if (instance) end(instance);
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
  #readHandle(name: string): unknown {
    this.#lent = undefined;
    this.#instance?.ports[name]();
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
              return this.graph.mockOutputs(
                what.node,
                (await options.mocks![what.node](inputs)) ?? ({} as PortValues),
              ) as PortValues;
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
    let ended = false;
    const instance = (await instantiate(
      (path: string) => modules.get(path)!,
      provided,
      this.#instantiateCore(),
      {
        // A call of `run` the trap ends rejects first, which names its node.
        onTrap: (error: unknown) => {
          setTimeout(() => {
            if (!ended && this.#instance === instance) this.graph.reportTrap(error);
          }, 0);
        },
      },
    )) as HubInstance;
    // An instance abandoned meanwhile is nobody's: a newer rebuild may already
    // have run on its own.
    if (!this.#disposed && this.#epoch === epoch) {
      this.#instance = instance;
      this.#taskGroups.clear();
    } else {
      ended = true;
      end(instance);
    }
  }

  /** What instantiates each core module, with the import its probes call, if it has any. */
  #instantiateCore():
    | ((module: WebAssembly.Module, imports: WebAssembly.Imports) => Promise<WebAssembly.Instance>)
    | undefined {
    const lines = this.#options.lines;
    const { probed } = this.#transpiled;
    if (!lines || probed.length === 0) return undefined;
    lines.instantiating?.();
    const instances = probed.map(() => 0);
    return async (module, imports) => {
      const at = probed.findIndex((entry) => entry.module === module);
      if (at < 0) return WebAssembly.instantiate(module, imports);
      const instance = instances[at]++;
      const made = await WebAssembly.instantiate(module, {
        ...imports,
        'witgraph:debug': { probe: (id: number) => lines.probe(at, instance, id) },
      });
      const flags = made.exports['witgraph:debug/flags'] as WebAssembly.Memory;
      const memory = Object.values(made.exports).find(
        (exported): exported is WebAssembly.Memory =>
          exported instanceof WebAssembly.Memory && exported !== flags,
      );
      lines.instance?.(at, instance, flags.buffer, memory);
      return made;
    };
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
    let returned: unknown;
    try {
      const running = instance.control.run(number);
      // In this turn, before anything else can have started.
      const group = instance._util.lastTaskGroup();
      if (group) this.#taskGroups.set(number, group);
      returned = await running;
    } catch (error) {
      // Ending the instance ends its calls.
      if (abandoned()) throw new Error(`the run of \`${node}\` was abandoned`);
      throw error;
    }
    if (abandoned()) return returned;
    this.#tap(node, instance);
    await options.afterRun?.(node);
    return returned;
  }

  /** When the instance has no guest work left, or none left by the last run of `nodes`. */
  #whenIdle(nodes?: NodeNumber[]): Promise<void> | undefined {
    const groups = nodes?.flatMap((node) => this.#taskGroups.get(node) ?? []);
    return this.#instance?._util.whenIdle(groups);
  }

  /**
   * Takes every stream and future the run of `node` delivered, and gives its
   * reader what reports it to the hooks. With no hooks they stay in the hub.
   */
  #tap(node: string, instance: HubInstance): void {
    const options = this.#options;
    if (!options.onStreamItem && !options.onFutureValue) return;
    const contract = this.contracts.get(node);
    const edges = this.graph.streamEdges(node) as {
      port: string;
      kind: 'stream' | 'future';
      take: string;
      give: string;
    }[];
    const live = () => !this.#disposed && this.#instance === instance;
    for (const { port, kind, take, give: giveName } of edges) {
      const value = instance.ports[take]();
      if (value === undefined) continue;
      const give = instance.ports[giveName];
      if (kind === 'future') {
        const future = value as PromiseLike<unknown>;
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
      give(
        tapped(value as AsyncIterable<unknown>, (item) => {
          if (!options.onStreamItem || this.#disposed) return;
          const items =
            numeric && ArrayBuffer.isView(item) ? (item as unknown as ArrayLike<unknown>) : [item];
          try {
            options.onStreamItem({ node, port, item, items });
          } catch (error) {
            console.error(`onStreamItem threw on \`${node}.${port}\`:`, error);
          }
        }),
      );
    }
  }
}

/** The ids of a graph's nodes, as its JSON has them. */
function mockableNodes(graph: string): string[] {
  const parsed = JSON.parse(graph) as { nodes?: { id: string }[] };
  return (parsed.nodes ?? []).map((node) => node.id);
}

/** Ends `instance`. What disposing the host objects it held throws is only logged. */
function end(instance: HubInstance): void {
  try {
    instance._util.stop();
  } catch (error) {
    console.error('disposing a handle threw:', error);
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
  'readInputWave',
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

/** `stream`, reporting each chunk as its reader takes it. */
function tapped(
  stream: AsyncIterable<unknown>,
  seen: (chunk: unknown) => void,
): AsyncIterable<unknown> {
  return {
    [Symbol.asyncIterator]() {
      const inner = stream[Symbol.asyncIterator]();
      return {
        async next() {
          const step = await inner.next();
          if (!step.done) seen(step.value);
          return step;
        },
        return: async () => inner.return?.() ?? { done: true, value: undefined },
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
function transpileCached(
  sha256: string,
  bytes: Uint8Array,
  lines: SessionLines | undefined,
): Promise<Transpiled> {
  // Probes make other modules of the same bytes.
  const key = lines ? `${sha256}:lines:${JSON.stringify(ownLines(lines))}` : sha256;
  let transpiled = transpileCache.get(key);
  if (transpiled === undefined) {
    transpiled = transpileComposite(bytes, lines);
    // A failed transpile is not kept.
    transpiled.catch(() => transpileCache.delete(key));
  } else {
    transpileCache.delete(key);
  }
  transpileCache.set(key, transpiled);
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

async function transpileComposite(
  bytes: Uint8Array,
  lines: SessionLines | undefined,
): Promise<Transpiled> {
  await $init;
  const { files, imports } = generate(bytes, TRANSPILE_OPTIONS);
  const modules = new Map<string, Promise<WebAssembly.Module>>();
  const probed: Transpiled['probed'] = [];
  let source: string | undefined;
  // jco makes each file's bytes itself, over an `ArrayBuffer`.
  for (const [name, contents] of files as [string, Uint8Array<ArrayBuffer>][]) {
    if (name.endsWith('.wasm')) {
      const rewritten =
        lines && (instrumentModule(contents, ownLines(lines)) as InstrumentedModule | null);
      const compiling = WebAssembly.compile(
        rewritten ? (rewritten.bytes as Uint8Array<ArrayBuffer>) : contents,
      );
      modules.set(name, compiling);
      if (rewritten) {
        probed.push({
          module: await compiling,
          probes: rewritten.probes,
          calls: rewritten.calls,
          sha256: rewritten.sha256,
          variables:
            moduleVariables(contents, rewritten.spill.base, rewritten.spill.globals) ?? undefined,
        });
      }
    } else if (name.endsWith('.js')) source = new TextDecoder().decode(contents);
  }
  if (source === undefined) throw new Error('the composite transpiled to no JavaScript');
  const url = `data:text/javascript;base64,${base64(new TextEncoder().encode(source))}`;
  const { instantiate } = await import(/* @vite-ignore */ url);
  return { instantiate, modules, imports, probed };
}

function base64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

/**
 * Something wrong with a graph, or worth a warning, as compiling says it. A failed
 * `Session.load` throws an error with these in its `diagnostics`, and a
 * {@link GraphChecker} returns them.
 */
export interface Diagnostic {
  /** Stable: what to tell diagnostics apart by, as `message` may change. */
  code?: string;
  severity: 'error' | 'warning';
  message: string;
  /** What to do about it, when there is something to say. */
  help?: string;
  location: {
    kind:
      | 'graph'
      | 'component'
      | 'node'
      | 'nodes'
      | 'port'
      | 'connection'
      | 'link'
      | 'cycle'
      | 'group';
    /** The nodes it touches: empty when it points at something else. */
    nodes: string[];
    port?: string;
    connection?: string;
    link?: string;
    component?: string;
  };
}

/** The diagnostics of what a failed `Session.load` threw, if it failed compiling. */
export function diagnosticsOf(error: unknown): Diagnostic[] | undefined {
  const diagnostics = (error as { diagnostics?: unknown } | null | undefined)?.diagnostics;
  return Array.isArray(diagnostics) ? (diagnostics as Diagnostic[]) : undefined;
}

export { AttachedSession, type AttachOptions } from './attach/session.ts';
export { fromWave, parseWave } from './attach/wave.ts';
export {
  type BreakpointScope,
  LineDebugger,
  type LineModule,
  type LineStop,
  type OwnLinesOptions,
  type SessionLines,
  type SourceProbe,
  type SourceVariable,
  type StackFrame,
} from './lines.ts';
export {
  type DebugGraph,
  type DebugLines,
  type DebugSession,
  describeHandle,
  type Endpoint,
  endWorker,
  type HandleItem,
  type HandleView,
  RemoteGraph,
  RemoteSession,
  type RemoteSessionOptions,
  type ServeOptions,
  type SessionWorker,
  serveSession,
} from './remote.ts';
export { GraphChecker, WebGraph };
