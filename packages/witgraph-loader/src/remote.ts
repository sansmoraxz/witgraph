// A session on another thread, so what its nodes do cannot hold up the page, and what
// the page does cannot hold up the nodes. `serveSession` runs in the worker and owns the
// `Session`; `RemoteSession` is the same session as the page sees it.
//
// Functions and resources stay where they were made: capabilities and handles live in
// the worker, and only values cross. What a page reads without waiting (a node's phase,
// an output's version) comes from the state the worker last reported, which a tick's
// reply and a `sync` always follow.

import {
  type Capability,
  type Contract,
  type Diagnostic,
  type Port,
  type PortValues,
  Session,
  type TickResult,
  type Wasm,
} from './index.ts';
import { LineDebugger, LineProbes, type LineStop, type LinesInfo } from './lines.ts';

/** A worker that can be ended, for what never returns to read another message. */
export type SessionWorker = Endpoint & { terminate?(): unknown };

/** How a session is reached: a `Worker`, a worker's global scope, or a `MessagePort`. */
export interface Endpoint {
  postMessage(message: unknown): void;
  // biome-ignore lint/suspicious/noExplicitAny: each kind of endpoint types its own events.
  addEventListener(type: string, listener: (event: any) => void): void;
  /** A `MessagePort` delivers nothing until this. */
  start?(): void;
}

/**
 * How the page is shown what a handle holds: a handle stays in the worker. As an item
 * passes, `keep` takes a reference of its own (it is its reader's, which may dispose it as
 * soon as it is done), and `read` makes of that what a viewer is given, a value that can
 * be cloned. The reference is disposed (`Symbol.dispose`) after. One read runs at a time
 * per output: what arrives meanwhile waits, the newest only, so a stream of handles is
 * sampled as fast as it reads back.
 */
export interface HandleView<Kept = unknown> {
  keep(handle: unknown): Kept;
  read(kept: Kept): unknown | Promise<unknown>;
}

/** What only the worker can provide. */
export interface ServeOptions {
  /** As {@link SessionOptions.capabilities}, each time the graph is instantiated. */
  capabilities?: (capability: Capability) => object | undefined;
  /** By the viewer an output names (`Port.viewer`): how it is shown of its handles. */
  // biome-ignore lint/suspicious/noExplicitAny: each view keeps what it keeps.
  views?: Record<string, HandleView<any>>;
}

/** A handle an output carried, as the page is shown it. */
export interface HandleItem {
  node: string;
  port: string;
  /** What the output's {@link HandleView} read, or else a description. */
  shown: unknown;
  /** How many items this stands for: those in between were not read. */
  count: number;
}

/** What {@link RemoteSession.load} loads, and what it tells the page. */
export interface RemoteSessionOptions {
  graph: object | string;
  components: { id: string; bytes: Uint8Array; wit?: string }[];
  /** The scheduler's module, for the worker. Only the first load on an endpoint sends it. */
  wasm?: Wasm;
  /** Called in the page, with inputs and outputs that can be cloned. */
  mocks?: Record<string, (inputs: PortValues) => PortValues | Promise<PortValues>>;
  /** Whose runs wait for {@link beforeRun}. Change it with {@link RemoteSession.holdAt}. */
  holds?: readonly string[] | 'all';
  /** Whose outputs are told of. Change it with {@link RemoteSession.observeOnly}. */
  observed?: readonly (readonly [node: string, port: string])[];
  /**
   * As a node's run starts. Only the runs `holds` names wait, for what this returns, as a
   * page that is busy must not hold up the rest: `held` says which this is.
   */
  beforeRun?: (node: string, held: boolean) => void | Promise<void>;
  /** As a node's run returns. It does not wait. */
  afterRun?: (node: string) => void;
  /** Items of an observed stream of values. */
  onStreamItem?: (item: { node: string; port: string; items: ArrayLike<unknown> }) => void;
  /** What an observed future of a value resolved to. */
  onFutureValue?: (item: { node: string; port: string; value: unknown }) => void;
  /** A handle an observed stream or future carried. */
  onHandle?: (item: HandleItem) => void;
  /** The worker reported state: {@link RemoteGraph.nodeState} and the rest may have moved. */
  onState?: () => void;
  /**
   * Debugs source lines: {@link RemoteSession.lines} sets breakpoints and steps, and `onStop`
   * says where a guest has stopped. It needs shared memory, so a page that is
   * cross-origin isolated, and a real worker, as a stop blocks the thread.
   */
  lines?: { all?: boolean; own?: readonly string[]; onStop: (stop: LineStop) => void };
}

/** The state a worker reports. */
interface State {
  nodes: Record<string, unknown>;
  versions: Record<string, number>;
  trace: unknown[];
  faults: unknown[];
  broken?: string;
}

interface WireError {
  name: string;
  message: string;
  diagnostics?: unknown;
}

/** What is on the page once a session has loaded. */
interface Loaded {
  contracts: [string, Contract][];
  warnings: string[];
  diagnostics: Diagnostic[];
  streamGroups: string[][];
  state: State;
  lines?: LinesInfo;
}

/** Marks this protocol's messages: a host may send a worker others of its own. */
const TAG = 'witgraph';

function wired(error: unknown): WireError {
  const { name, message, diagnostics } = error as Partial<WireError>;
  return {
    name: typeof name === 'string' ? name : 'Error',
    message: typeof message === 'string' ? message : String(error),
    ...(Array.isArray(diagnostics) ? { diagnostics } : {}),
  };
}

function unwired(error: WireError): Error {
  const rebuilt = new Error(error.message);
  rebuilt.name = error.name;
  if (error.diagnostics) (rebuilt as { diagnostics?: unknown }).diagnostics = error.diagnostics;
  return rebuilt;
}

/** How one output's value is known by: ports are never named with a NUL. */
const portKey = (node: string, port: string) => `${node}\0${port}`;

/**
 * What crosses of a stream's items. Cloning a view clones all of its buffer, which may be
 * far more than the view, so such a view is copied down to itself first.
 */
function detached(items: ArrayLike<unknown>): ArrayLike<unknown> {
  if (!ArrayBuffer.isView(items)) return items;
  const view = items as unknown as ArrayLike<unknown> & {
    buffer: ArrayBufferLike;
    byteLength: number;
    slice(): ArrayLike<unknown>;
  };
  return view.byteLength === view.buffer.byteLength ? view : view.slice();
}

/** What a handle looks like with no view of it. */
export function describeHandle(value: unknown, resource: string): string {
  const own = (value as { toString?: unknown } | null)?.toString;
  return typeof own === 'function' && own !== Object.prototype.toString
    ? String(value)
    : `<${resource}>`;
}

const dispose = (kept: unknown) => {
  try {
    (kept as Partial<Disposable> | null | undefined)?.[Symbol.dispose]?.();
  } catch (error) {
    console.error('disposing what a handle view kept threw:', error);
  }
};

/** The promise-returning calls of the graph a page may make, each as it is. */
const GRAPH_CALLS = new Set([
  'inject',
  'injectWave',
  'clearInput',
  'readInput',
  'readOutput',
  'readOutputWave',
  'snapshot',
  'restore',
  'rerun',
  'shutdown',
]);

/** The calls of the graph that return at once, and that nothing waits on. */
const GRAPH_POSTS = new Set(['catchUp']);

/** How often state is reported while a tick runs, whatever else happens. */
const REPORT_EVERY_MS = 50;
/** How long after something happened state is reported, so a burst reports once. */
const REPORT_SOON_MS = 16;

/** A session of one page's, in the worker. */
class Served {
  #send: (message: object) => void;
  #id: number;
  #options: ServeOptions;
  session!: Session;
  #outputs = new Map<string, Port>();
  #holds: Set<string> | 'all' = new Set();
  #observed = new Set<string>();
  /** By hold: what lets a held run go on. */
  #held = new Map<number, () => void>();
  #stops = new Map<number, () => void>();
  /** By output: the read of its handles under way, and the newest waiting for it. */
  #reading = new Map<string, { waiting?: unknown; passed: number }>();
  #ticking = 0;
  #every: ReturnType<typeof setInterval> | undefined;
  #soon: ReturnType<typeof setTimeout> | undefined;
  #disposed = false;
  #lines: LineProbes | undefined;

  constructor(send: (message: object) => void, id: number, options: ServeOptions) {
    this.#send = send;
    this.#id = id;
    this.#options = options;
  }

  #post(kind: string, fields: object): void {
    this.#send({ [TAG]: kind, session: this.#id, ...fields });
  }

  holdAt(holds: readonly string[] | 'all'): void {
    this.#holds = holds === 'all' ? 'all' : new Set(holds);
  }

  observeOnly(ports: readonly (readonly [string, string])[]): void {
    this.#observed = new Set(ports.map(([node, port]) => portKey(node, port)));
  }

  /** Lets a held run go on. */
  release(hold: number): void {
    this.#held.get(hold)?.();
    this.#held.delete(hold);
  }

  stop(tick: number): void {
    this.#stops.get(tick)?.();
  }

  async load(
    message: {
      graph: object | string;
      components: { id: string; bytes: Uint8Array; wit?: string }[];
      wasm?: Wasm | { url: string };
      mocked: string[];
      holds: readonly string[] | 'all';
      observed: readonly (readonly [string, string])[];
      lines?: { all?: boolean; own?: readonly string[] };
    },
    ask: (kind: string, fields: object) => Promise<unknown>,
  ): Promise<Loaded> {
    this.holdAt(message.holds);
    this.observeOnly(message.observed);
    const wasm = message.wasm;
    if (message.lines) {
      this.#lines = new LineProbes(
        (kind, fields) => this.#post(kind, fields),
        () => this.report(),
        message.lines,
      );
    }
    const mocks = Object.fromEntries(
      message.mocked.map((node) => [
        node,
        (inputs: PortValues) =>
          ask('mock', { session: this.#id, node, inputs }) as Promise<PortValues>,
      ]),
    );
    this.session = await Session.load({
      graph: message.graph,
      components: message.components,
      ...(wasm === undefined
        ? {}
        : { wasm: 'url' in wasm ? new URL((wasm as { url: string }).url) : (wasm as Wasm) }),
      capabilities: this.#options.capabilities,
      lines: this.#lines,
      mocks,
      beforeRun: (node) => this.#beforeRun(node),
      afterRun: (node) => {
        this.#post('after', { node });
        this.#reportSoon();
      },
      onStreamItem: ({ node, port, items }) => {
        if (!this.#observed.has(portKey(node, port))) return;
        const carried = this.#outputs.get(portKey(node, port));
        if (carried?.handle) {
          for (const item of Array.from(items)) this.#passed(node, port, carried, item);
        } else {
          this.#post('stream', { node, port, items: detached(items) });
        }
      },
      onFutureValue: ({ node, port, value }) => {
        if (!this.#observed.has(portKey(node, port))) return;
        const carried = this.#outputs.get(portKey(node, port));
        if (carried?.handle) this.#passed(node, port, carried, value);
        else this.#post('future', { node, port, value });
      },
    });
    for (const [node, contract] of this.session.contracts) {
      for (const output of contract.outputs) this.#outputs.set(portKey(node, output.name), output);
    }
    return {
      contracts: [...this.session.contracts],
      warnings: [...this.session.warnings],
      diagnostics: [...this.session.diagnostics],
      streamGroups: this.session.graph.streamGroups() as string[][],
      state: this.#state(),
      lines: this.#lines?.info(),
    };
  }

  #beforeRun(node: string): void | Promise<void> {
    const held =
      this.#holds === 'all' || this.#holds.has(node) || this.#lines?.holdingNodes === true;
    this.#reportSoon();
    if (!held) {
      this.#post('before', { node });
      return;
    }
    // The page shows what it is paused at: with what has run by then.
    this.report();
    return new Promise((resolve) => {
      const hold = this.#nextHold++;
      this.#held.set(hold, resolve);
      this.#post('before', { node, hold });
    });
  }

  #nextHold = 0;

  /** A handle passing an observed output. */
  #passed(node: string, port: string, carried: Port, item: unknown): void {
    const view = carried.viewer ? this.#options.views?.[carried.viewer] : undefined;
    if (!view) {
      const resource = carried.handle?.resource ?? 'handle';
      this.#post('handle', { node, port, shown: describeHandle(item, resource), count: 1 });
      return;
    }
    const kept = view.keep(item);
    const key = portKey(node, port);
    const reading = this.#reading.get(key);
    if (reading) {
      dispose(reading.waiting);
      reading.waiting = kept;
      reading.passed += 1;
      return;
    }
    const started = { passed: 0 } as { waiting?: unknown; passed: number };
    this.#reading.set(key, started);
    void this.#read(node, port, view, kept, started, 1);
  }

  /** Shows the page `kept`, then whatever waited for that. */
  async #read(
    node: string,
    port: string,
    view: HandleView,
    first: unknown,
    reading: { waiting?: unknown; passed: number },
    firstCount: number,
  ): Promise<void> {
    let kept = first;
    let count = firstCount;
    for (;;) {
      try {
        const shown = await view.read(kept);
        if (!this.#disposed) this.#post('handle', { node, port, shown, count });
      } catch (error) {
        // A read may fail, as a backend's may: the next one shows.
        console.warn(`witgraph: reading a handle of \`${node}.${port}\` failed:`, error);
      } finally {
        dispose(kept);
      }
      if (this.#disposed || reading.waiting === undefined) break;
      kept = reading.waiting;
      count = reading.passed;
      reading.waiting = undefined;
      reading.passed = 0;
    }
    this.#reading.delete(portKey(node, port));
  }

  /** What a handle output holds, as the page is shown it. */
  async readHandle(node: string, port: string): Promise<unknown> {
    const lent = await this.session.graph.readHandle(node, port);
    if (lent === undefined) return undefined;
    const carried = this.#outputs.get(portKey(node, port));
    const view = carried?.viewer ? this.#options.views?.[carried.viewer] : undefined;
    if (!view) return describeHandle(lent, carried?.handle?.resource ?? 'handle');
    const kept = view.keep(lent);
    try {
      return await view.read(kept);
    } finally {
      dispose(kept);
    }
  }

  #state(): State {
    const graph = this.session.graph;
    const broken = this.session.broken;
    if (broken) {
      return { nodes: {}, versions: {}, trace: [], faults: [], broken: broken.message };
    }
    const nodes: Record<string, unknown> = {};
    const versions: Record<string, number> = {};
    for (const node of this.session.contracts.keys()) {
      try {
        nodes[node] = graph.nodeState(node);
      } catch {}
    }
    for (const [key, output] of this.#outputs) {
      if (output.kind !== 'value' && output.kind !== 'handle') continue;
      const [node, port] = key.split('\0');
      try {
        versions[key] = graph.outputVersion(node, port);
      } catch {}
    }
    return {
      nodes,
      versions,
      trace: graph.takeTrace() as unknown[],
      // A tick reports its own, and has the scheduler until it ends.
      faults: this.#ticking > 0 ? [] : (graph.takeFaults() as unknown[]),
    };
  }

  report(): void {
    clearTimeout(this.#soon);
    this.#soon = undefined;
    if (this.#disposed) return;
    this.#post('state', { state: this.#state() });
  }

  #reportSoon(): void {
    if (this.#soon === undefined) this.#soon = setTimeout(() => this.report(), REPORT_SOON_MS);
  }

  /** A tick: its state is on its way before its reply is. */
  async tick(id: number, stoppable: boolean): Promise<TickResult> {
    const stopped = stoppable
      ? new Promise<void>((resolve) => this.#stops.set(id, resolve))
      : undefined;
    this.#ticking += 1;
    this.#every ??= setInterval(() => this.report(), REPORT_EVERY_MS);
    try {
      return await this.session.tick(stopped);
    } finally {
      this.#stops.delete(id);
      this.#ticking -= 1;
      if (this.#ticking === 0) {
        clearInterval(this.#every);
        this.#every = undefined;
      }
      this.report();
    }
  }

  call(method: string, args: unknown[]): unknown {
    if (method === 'readInputs') return this.session.readInputs(args[0] as string);
    if (method === 'readHandle') return this.readHandle(args[0] as string, args[1] as string);
    if (method === 'sync') return void this.report();
    if (!GRAPH_CALLS.has(method)) throw new Error(`a session has no call \`${method}\``);
    const done = (this.session.graph as unknown as Record<string, (...args: unknown[]) => unknown>)[
      method
    ](...args);
    // What they change is told as soon as it is done, as it is for what the session runs.
    if (method === 'rerun' || method === 'shutdown') {
      return Promise.resolve(done).finally(() => this.#reportSoon());
    }
    return done;
  }

  /** The calls nothing waits on: a failed one is as if not made. */
  post(method: string, args: unknown[]): void {
    if (!GRAPH_POSTS.has(method)) return;
    try {
      (this.session.graph as unknown as Record<string, (...args: unknown[]) => unknown>)[method](
        ...args,
      );
    } catch {}
    this.#reportSoon();
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    clearInterval(this.#every);
    clearTimeout(this.#soon);
    // A held run stops at its next step: the session is gone.
    for (const go of this.#held.values()) go();
    this.#held.clear();
    for (const stop of this.#stops.values()) stop();
    for (const reading of this.#reading.values()) dispose(reading.waiting);
    this.session?.dispose();
  }
}

/**
 * Serves sessions over `endpoint`: call this in a worker, with its global scope. Call it
 * as the worker starts: `options` may be a promise, for what takes time to set up.
 */
export function serveSession(
  endpoint: Endpoint,
  options: ServeOptions | Promise<ServeOptions> = {},
): void {
  const ready = Promise.resolve(options);
  const sessions = new Map<number, Served>();
  const asked = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>();
  let nextAsk = 0;
  const send = (message: object) => {
    try {
      endpoint.postMessage(message);
    } catch (error) {
      const { id, session, node, port } = message as Record<string, unknown>;
      if (id !== undefined) {
        endpoint.postMessage({ [TAG]: 'reply', id, error: wired(error) });
      } else {
        // Nothing waits for it: the page is only told of less.
        console.warn(`witgraph: \`${node}.${port}\` of session ${session} did not cross:`, error);
      }
    }
  };
  const ask = (kind: string, fields: object) =>
    new Promise<unknown>((resolve, reject) => {
      const id = nextAsk++;
      asked.set(id, { resolve, reject });
      send({ [TAG]: kind, id, ...fields });
    });
  const reply = (id: number, work: () => unknown) => {
    void (async () => {
      try {
        send({ [TAG]: 'reply', id, value: await work() });
      } catch (error) {
        send({ [TAG]: 'reply', id, error: wired(error) });
      }
    })();
  };

  // biome-ignore lint/suspicious/noExplicitAny: a message is checked by its tag.
  endpoint.addEventListener('message', ({ data }: { data: any }) => {
    const kind = data?.[TAG];
    if (kind === undefined) return;
    const served = sessions.get(data.session);
    switch (kind) {
      case 'load':
        reply(data.id, async () => {
          const session = new Served(send, data.session, await ready);
          const loaded = await session.load(data, ask);
          sessions.set(data.session, session);
          return loaded;
        });
        break;
      case 'tick':
        reply(data.id, () => {
          if (!served) throw new Error('the session is disposed');
          return served.tick(data.id, data.stoppable);
        });
        break;
      case 'call':
        reply(data.id, () => {
          if (!served) throw new Error('the session is disposed');
          return served.call(data.method, data.args);
        });
        break;
      case 'post':
        served?.post(data.method, data.args);
        break;
      case 'stop':
        served?.stop(data.tick);
        break;
      case 'holds':
        served?.holdAt(data.holds);
        break;
      case 'observe':
        served?.observeOnly(data.ports);
        break;
      case 'release':
        served?.release(data.hold);
        break;
      case 'answer': {
        const waiting = asked.get(data.id);
        asked.delete(data.id);
        if (data.error) waiting?.reject(unwired(data.error));
        else waiting?.resolve(data.value);
        break;
      }
      case 'dispose':
        served?.dispose();
        sessions.delete(data.session);
        break;
    }
  });
  endpoint.start?.();
}

/** One endpoint's traffic, shared by the sessions on it. */
class Channel {
  endpoint: Endpoint;
  #replies = new Map<
    number,
    { resolve(value: unknown): void; reject(error: Error): void; load: boolean }
  >();
  #nextId = 0;
  sessions = new Map<number, RemoteSession>();
  nextSession = 0;
  sentWasm = false;

  constructor(endpoint: Endpoint) {
    this.endpoint = endpoint;
    // biome-ignore lint/suspicious/noExplicitAny: a message is checked by its tag.
    endpoint.addEventListener('message', ({ data }: { data: any }) => this.#received(data));
    // A worker that does not even start would leave a load waiting for good. Later, what
    // it throws is not known to end anything: a session that is broken says so itself.
    endpoint.addEventListener('error', (event: { message?: string }) => {
      for (const [id, waiting] of this.#replies) {
        if (!waiting.load) continue;
        this.#replies.delete(id);
        waiting.reject(new Error(`the session's worker failed: ${event.message ?? 'no reason'}`));
      }
    });
    endpoint.start?.();
  }

  post(kind: string, fields: object): void {
    this.endpoint.postMessage({ [TAG]: kind, ...fields });
  }

  /** The worker is gone: nothing it was asked will be answered. */
  close(): void {
    for (const session of this.sessions.values()) session.abandon();
    this.sessions.clear();
    const replies = [...this.#replies.values()];
    this.#replies.clear();
    for (const { reject } of replies) reject(new Error("the session's worker was ended"));
  }

  ask(kind: string, fields: object): Promise<unknown> {
    return this.asked(kind, fields).reply;
  }

  /** As {@link ask}, with the id the worker knows the request by. */
  asked(kind: string, fields: object): { id: number; reply: Promise<unknown> } {
    const id = this.#nextId++;
    const reply = new Promise((resolve, reject) => {
      this.#replies.set(id, { resolve, reject, load: kind === 'load' });
      this.post(kind, { id, ...fields });
    });
    return { id, reply };
  }

  // biome-ignore lint/suspicious/noExplicitAny: a message is checked by its tag.
  #received(data: any): void {
    const kind = data?.[TAG];
    if (kind === undefined) return;
    if (kind === 'reply') {
      const waiting = this.#replies.get(data.id);
      this.#replies.delete(data.id);
      if (data.error) waiting?.reject(unwired(data.error));
      else waiting?.resolve(data.value);
      return;
    }
    this.sessions.get(data.session)?.received(kind, data);
  }
}

const channels = new WeakMap<Endpoint, Channel>();

/**
 * Terminates a worker, the only way to end a node that spins, since the worker never gets
 * to read a `shutdown`. Its sessions go with it, and whatever waits on them rejects. Says
 * whether it could: an endpoint with no `terminate` is left as it is.
 */
export function endWorker(worker: SessionWorker): boolean {
  if (!worker.terminate) return false;
  worker.terminate();
  channels.get(worker)?.close();
  channels.delete(worker);
  return true;
}

/** The scheduler of a session in a worker, as {@link Session.graph} is on this thread. */
export class RemoteGraph {
  #session: RemoteSession;
  #groups: string[][];

  /** @internal */
  constructor(session: RemoteSession, groups: string[][]) {
    this.#session = session;
    this.#groups = groups;
  }

  #call<T>(method: string, ...args: unknown[]): Promise<T> {
    return this.#session.call(method, args) as Promise<T>;
  }

  inject(node: string, port: string, value: unknown): Promise<void> {
    return this.#call('inject', node, port, value);
  }

  injectWave(node: string, port: string, wave: string): Promise<void> {
    return this.#call('injectWave', node, port, wave);
  }

  clearInput(node: string, port: string): Promise<void> {
    return this.#call('clearInput', node, port);
  }

  readInput(node: string, port: string): Promise<unknown> {
    return this.#call('readInput', node, port);
  }

  readOutput(node: string, port: string): Promise<unknown> {
    return this.#call('readOutput', node, port);
  }

  readOutputWave(node: string, port: string): Promise<string | undefined> {
    return this.#call('readOutputWave', node, port);
  }

  /** What a handle output holds, as its {@link HandleView} shows it, or a description. */
  readHandle(node: string, port: string): Promise<unknown> {
    return this.#call('readHandle', node, port);
  }

  snapshot(): Promise<string> {
    return this.#call('snapshot');
  }

  restore(snapshot: string): Promise<void> {
    return this.#call('restore', snapshot);
  }

  /** A node's state, as last reported. */
  nodeState(node: string): unknown {
    const state = this.#session.state.nodes[node];
    if (state === undefined) throw new Error(`no node \`${node}\``);
    return state;
  }

  /** Which value an output latches, as last reported. */
  outputVersion(node: string, port: string): number {
    const version = this.#session.state.versions[portKey(node, port)];
    if (version === undefined) throw new Error(`no output \`${node}.${port}\` to read`);
    return version;
  }

  /** The trace reported since this was last asked. */
  takeTrace(): unknown[] {
    return this.#session.state.trace.splice(0);
  }

  /** The faults reported since this was last asked. */
  takeFaults(): unknown[] {
    return this.#session.state.faults.splice(0);
  }

  streamGroups(): string[][] {
    return this.#groups;
  }

  /** The node owes a run. In order with what else is asked of the session. */
  rerun(node: string): Promise<void> {
    return this.#call('rerun', node);
  }

  /** Ends what is running. In order with what else is asked of the session. */
  shutdown(): Promise<void> {
    return this.#call('shutdown');
  }

  /** Takes in what is reported between ticks. In order with what else is asked. */
  catchUp(): void {
    this.#session.postGraph('catchUp', []);
  }

  /** Waits for the state as it is now, in order with what else was asked. */
  sync(): Promise<void> {
    return this.#call('sync');
  }
}

/** A graph loaded in a worker: a {@link Session}, from another thread. */
export class RemoteSession {
  readonly graph: RemoteGraph;
  readonly contracts: Map<string, Contract>;
  readonly warnings: readonly string[];
  readonly diagnostics: readonly Diagnostic[];
  /** Sets breakpoints and steps, in a session loaded with `lines`. */
  readonly lines: LineDebugger | undefined;
  /** @internal */
  state: State;
  #channel: Channel;
  #id: number;
  #options: RemoteSessionOptions;
  #disposed = false;

  private constructor(channel: Channel, id: number, options: RemoteSessionOptions, loaded: Loaded) {
    this.#channel = channel;
    this.#id = id;
    this.#options = options;
    this.state = loaded.state;
    this.contracts = new Map(loaded.contracts);
    this.warnings = loaded.warnings;
    this.diagnostics = loaded.diagnostics;
    this.graph = new RemoteGraph(this, loaded.streamGroups);
    this.lines =
      loaded.lines && options.lines && new LineDebugger(loaded.lines, options.lines.onStop);
  }

  /** Loads the graph in the worker `endpoint` serves. */
  static async load(endpoint: Endpoint, options: RemoteSessionOptions): Promise<RemoteSession> {
    if (
      options.lines &&
      (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated === false
    ) {
      throw new Error(
        'debugging source lines needs shared memory: serve the page with the headers ' +
          '`Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`',
      );
    }
    let channel = channels.get(endpoint);
    if (!channel) {
      channel = new Channel(endpoint);
      channels.set(endpoint, channel);
    }
    const id = channel.nextSession++;
    // What is reported before `load` has returned waits for the session it is of.
    const early: [string, object][] = [];
    const placeholder = {
      received: (kind: string, data: object) => early.push([kind, data]),
    } as unknown as RemoteSession;
    channel.sessions.set(id, placeholder);
    const wasm = channel.sentWasm ? undefined : options.wasm;
    const mocks = options.mocks ?? {};
    let loaded: Loaded;
    try {
      loaded = (await channel.ask('load', {
        session: id,
        graph: typeof options.graph === 'string' ? options.graph : JSON.stringify(options.graph),
        components: options.components,
        ...(wasm === undefined ? {} : { wasm: wasm instanceof URL ? { url: wasm.href } : wasm }),
        mocked: Object.keys(mocks).filter((node) => typeof mocks[node] === 'function'),
        holds: options.holds ?? [],
        observed: options.observed ?? [],
        ...(options.lines ? { lines: { all: options.lines.all, own: options.lines.own } } : {}),
      })) as Loaded;
    } catch (error) {
      channel.sessions.delete(id);
      throw error;
    }
    channel.sentWasm = true;
    const session = new RemoteSession(channel, id, options, loaded);
    channel.sessions.set(id, session);
    for (const [kind, data] of early) session.received(kind, data);
    return session;
  }

  /** Why the session can no longer be used, once the scheduler has trapped. */
  get broken(): Error | undefined {
    return this.state.broken === undefined ? undefined : new Error(this.state.broken);
  }

  /** @internal */
  // biome-ignore lint/suspicious/noExplicitAny: a message is checked by its tag.
  received(kind: string, data: any): void {
    if (this.#disposed) return;
    if (kind.startsWith('lines-')) {
      this.lines?.received(kind, data);
      return;
    }
    const options = this.#options;
    switch (kind) {
      case 'state': {
        // What was not yet taken stays, until it is.
        const { trace, faults } = this.state;
        this.state = {
          ...data.state,
          trace: trace.concat(data.state.trace),
          faults: faults.concat(data.state.faults),
        };
        options.onState?.();
        break;
      }
      case 'before': {
        const held = data.hold !== undefined;
        const asked = options.beforeRun?.(data.node, held);
        if (held) {
          const release = () => this.#post('release', { hold: data.hold });
          Promise.resolve(asked).then(release, release);
        }
        break;
      }
      case 'after':
        options.afterRun?.(data.node);
        break;
      case 'stream':
        options.onStreamItem?.({ node: data.node, port: data.port, items: data.items });
        break;
      case 'future':
        options.onFutureValue?.({ node: data.node, port: data.port, value: data.value });
        break;
      case 'handle':
        options.onHandle?.({
          node: data.node,
          port: data.port,
          shown: data.shown,
          count: data.count,
        });
        break;
      case 'mock':
        void (async () => {
          try {
            const value = await options.mocks?.[data.node](data.inputs);
            this.#channel.post('answer', { id: data.id, value });
          } catch (error) {
            this.#channel.post('answer', { id: data.id, error: wired(error) });
          }
        })();
        break;
    }
  }

  #post(kind: string, fields: object): void {
    if (!this.#disposed) this.#channel.post(kind, { session: this.#id, ...fields });
  }

  /** @internal */
  abandon(): void {
    this.#disposed = true;
  }

  /** @internal */
  call(method: string, args: unknown[]): Promise<unknown> {
    if (this.#disposed) return Promise.reject(new Error('the session is disposed'));
    return this.#channel.ask('call', { session: this.#id, method, args });
  }

  /** @internal */
  postGraph(method: string, args: unknown[]): void {
    this.#post('post', { method, args });
  }

  /** Whose runs wait for `beforeRun`, from the next to start. */
  holdAt(nodes: readonly string[] | 'all'): void {
    this.#post('holds', { holds: nodes });
  }

  /** Whose outputs are told of, so that nothing else crosses. */
  observeOnly(ports: readonly (readonly [node: string, port: string])[]): void {
    this.#post('observe', { ports });
  }

  /** As {@link Session.tick}. */
  tick(stop?: Promise<unknown>): Promise<TickResult> {
    if (this.#disposed) return Promise.reject(new Error('the session is disposed'));
    const { id, reply } = this.#channel.asked('tick', {
      session: this.#id,
      stoppable: stop !== undefined,
    });
    void stop?.then(() => this.#post('stop', { tick: id }));
    return reply as Promise<TickResult>;
  }

  /** As {@link Session.settle}. */
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

  /** As {@link Session.readInputs}. */
  readInputs(node: string): Promise<PortValues> {
    return this.call('readInputs', [node]) as Promise<PortValues>;
  }

  /** As {@link Session.dispose}. The worker goes on, for the next session. */
  dispose(): void {
    if (this.#disposed) return;
    // A guest stopped on a line holds up the worker, and so what this asks of it.
    this.lines?.release();
    this.#post('dispose', {});
    this.#disposed = true;
    this.#channel.sessions.delete(this.#id);
  }
}
