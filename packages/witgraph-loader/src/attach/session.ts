/**
 * A session attached to a graph running on wasmtime elsewhere, over wRPC on WebTransport: what
 * `witgraph-remote` serves. The host program ticks the graph; this holds nodes, stops on lines,
 * reads outputs and writes inputs, and cannot change the graph.
 */

import { fromWebStreams, type Transport } from '@bytecodealliance/wrpc';
import type { Contract, Diagnostic, PortValues, TickResult } from '../index.ts';
import type { LineStop, SourceVariable } from '../lines.ts';
import type { DebugGraph, DebugLines, DebugSession } from '../remote.ts';
import * as graph from './bindings/witgraph/debug/graph.js';
import * as holds from './bindings/witgraph/debug/holds.js';
import * as lines from './bindings/witgraph/debug/lines.js';

/** Where to attach, as the program serving the graph printed it. */
export interface AttachOptions {
  /** The URL, with its `token`. */
  url: string;
  /** The SHA-256 of the server's certificate, which is self-signed. */
  certificateHash: Uint8Array<ArrayBuffer>;
  /** A run held before it starts: it goes on once what this returns settles. */
  beforeRun?: (node: string, held: boolean) => void | Promise<void>;
  /** A guest stopped on a line. */
  onStop?: (stop: LineStop) => void;
  /** Node states moved. */
  onState?: () => void;
  /** How often node states are asked for, in milliseconds. 250 by default. */
  pollEvery?: number;
}

interface State {
  phase: string;
  fault?: { kind: string; message: string };
  culprit?: string;
}

/** A session attached to a graph on wasmtime. */
export class AttachedSession implements DebugSession {
  readonly graph: DebugGraph;
  readonly lines: DebugLines | undefined;
  readonly contracts: Map<string, Contract>;
  readonly warnings: readonly string[] = [];
  readonly diagnostics: readonly Diagnostic[] = [];
  /** The graph, as the JSON `witgraph_ir::Graph` serializes to. */
  readonly described: string;
  readonly #connection: WebTransport;
  readonly #options: AttachOptions;
  #disposed = false;
  #timer: ReturnType<typeof setInterval> | undefined;
  /** The held runs told of: each once. */
  readonly #toldHeld = new Set<bigint>();
  /** The stop told of, until the guest goes on: told once, by whichever way it came. */
  readonly #toldStop: { stop?: string } = {};

  private constructor(
    connection: WebTransport,
    described: string,
    contracts: Map<string, Contract>,
    groups: string[][],
    sources: Map<string, number[]>,
    options: AttachOptions,
  ) {
    this.#connection = connection;
    this.#options = options;
    this.described = described;
    this.contracts = contracts;
    const open = () => this.#open();
    const states = new Map<string, State>();
    const versions = new Map<string, number>();
    const poll = async () => {
      let moved = false;
      await Promise.all(
        [...contracts.keys()].map(async (node) => {
          const state = await graph.nodeState(await open(), node).catch(() => undefined);
          if (!state || state.kind === 'error') return;
          const { phase, fault, culprit } = state.value;
          const next: State = {
            phase,
            fault: fault === undefined ? undefined : { kind: 'fault', message: fault },
            culprit,
          };
          if (JSON.stringify(states.get(node)) !== JSON.stringify(next)) moved = true;
          states.set(node, next);
          for (const port of contracts.get(node)?.outputs ?? []) {
            const key = `${node}\0${port.name}`;
            const version = await graph
              .outputVersion(await open(), node, port.name)
              .catch(() => undefined);
            if (!version || version.kind === 'error') continue;
            if (versions.get(key) !== Number(version.value)) moved = true;
            versions.set(key, Number(version.value));
          }
        }),
      );
      if (moved) options.onState?.();
      // What a stream of them could have lost: to a debugger gone before this, still taking
      // them until its connection times out.
      const [held, stopped] = await Promise.all([
        open()
          .then(holds.held)
          .catch(() => []),
        this.lines
          ? open()
              .then(lines.stopped)
              .catch(() => undefined)
          : undefined,
      ]);
      for (const run of held) this.#tellHeld(run);
      if (stopped) this.#tellStop(stopped);
    };
    this.#timer = setInterval(() => void poll(), options.pollEvery ?? 250);
    void poll();
    this.graph = attachedGraph(open, states, versions, groups, poll);
    this.lines = sources.size > 0 ? attachedLines(open, sources, this.#toldStop) : undefined;
    void this.#follow();
  }

  /** Attaches to the graph `url` serves. */
  static async attach(options: AttachOptions): Promise<AttachedSession> {
    const connection = new WebTransport(options.url, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: options.certificateHash }],
    });
    await connection.ready;
    const open = async () => fromWebStreams(await connection.createBidirectionalStream());
    const [described, contracts, groups, files] = await Promise.all([
      open().then(graph.describe),
      open().then(graph.contracts),
      open().then(graph.streamGroups),
      open().then(lines.files),
    ]);
    const sources = new Map<string, number[]>();
    for (const file of files) sources.set(file, [...(await lines.lines(await open(), file))]);
    return new AttachedSession(
      connection,
      described,
      new Map(Object.entries(JSON.parse(contracts) as Record<string, Contract>)),
      groups,
      sources,
      options,
    );
  }

  /** A file's text, as the host has it. */
  async source(file: string): Promise<string | undefined> {
    return lines.source(await this.#open(), file);
  }

  async #open(): Promise<Transport> {
    return fromWebStreams(await this.#connection.createBidirectionalStream());
  }

  /** Tells of a held run, once: it goes on once the debugger is done with it. */
  #tellHeld(run: holds.HeldRun): void {
    if (this.#toldHeld.has(run.id)) return;
    this.#toldHeld.add(run.id);
    void Promise.resolve(this.#options.beforeRun?.(run.node, true)).then(async () => {
      if (!this.#disposed) await holds.resume(await this.#open(), run.id);
    });
  }

  /** Tells of a stop, once until the guest goes on. */
  #tellStop(stop: lines.LineStop): void {
    const key = JSON.stringify(stop);
    if (this.#toldStop.stop === key) return;
    this.#toldStop.stop = key;
    this.#options.onStop?.(lineStop(stop));
  }

  /** Tells of each run held and each stop on a line as it comes, until disposed. */
  async #follow(): Promise<void> {
    const held = async () => {
      while (!this.#disposed) {
        const run = await holds.nextHeld(await this.#open()).catch((error: unknown) => {
          if (!this.#disposed) console.warn('witgraph: no more held runs are told of:', error);
          return undefined;
        });
        if (!run) return;
        this.#tellHeld(run);
      }
    };
    const stops = async () => {
      if (!this.lines) return;
      while (!this.#disposed) {
        const stop = await lines.nextStop(await this.#open()).catch((error: unknown) => {
          if (!this.#disposed) console.warn('witgraph: no more stops on lines are told of:', error);
          return undefined;
        });
        if (!stop) return;
        this.#tellStop(stop);
      }
    };
    await Promise.all([held(), stops()]);
  }

  holdAt(nodes: readonly string[] | 'all'): void {
    const at: holds.Holding =
      nodes === 'all' ? { kind: 'all' } : { kind: 'nodes', value: [...nodes] };
    void this.#open().then((transport) => holds.holdAt(transport, at));
  }

  /** The host's previews are its own: nothing is observed from here. */
  observeOnly(): void {}

  /** wasmtime has no way to read a latched input as text yet. */
  async readInputs(): Promise<PortValues> {
    return {};
  }

  /** The program serving the graph ticks it. */
  tick(): Promise<TickResult> {
    return Promise.reject(new Error('an attached graph is ticked by the program running it'));
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    clearInterval(this.#timer);
    // Nothing is left held or stopped by a debugger that has gone.
    void this.#open()
      .then((transport) => holds.release(transport))
      .then(() => this.#open())
      .then((transport) => lines.release(transport))
      .catch(() => {})
      .finally(() => this.#connection.close());
  }
}

/** What a wRPC function returning `result<T, string>` gives. */
type Outcome<T> = { kind: 'ok'; value: T } | { kind: 'error'; value: string };

/** Calls a wRPC function returning `result<T, string>`, as a promise of `T`. */
async function ok<T>(called: Promise<Outcome<T>>): Promise<T> {
  const result = await called;
  if (result.kind === 'error') throw new Error(result.value);
  return result.value;
}

/** The same, for `result<_, string>`. */
async function done(
  called: Promise<{ kind: 'ok' } | { kind: 'error'; value: string }>,
): Promise<void> {
  const result = await called;
  if (result.kind === 'error') throw new Error(result.value);
}

function attachedGraph(
  open: () => Promise<Transport>,
  states: Map<string, State>,
  versions: Map<string, number>,
  groups: string[][],
  poll: () => Promise<void>,
): DebugGraph {
  const notAttached = (what: string) =>
    Promise.reject(new Error(`${what} is the program's to do, not an attached debugger's`));
  return {
    injectWave: async (node, port, wave) => done(graph.inject(await open(), node, port, wave)),
    clearInput: async (node, port) => done(graph.clearInput(await open(), node, port)),
    rerun: async (node) => done(graph.rerun(await open(), node)),
    // Values cross as WAVE text: a host's values are its own.
    readOutput: async (node, port) => ok(graph.readOutput(await open(), node, port)),
    readOutputWave: async (node, port) => ok(graph.readOutput(await open(), node, port)),
    readHandle: async (node, port) => ok(graph.readOutput(await open(), node, port)),
    nodeState: (node) => {
      const state = states.get(node);
      if (!state) throw new Error(`no state of \`${node}\` yet`);
      return state;
    },
    outputVersion: (node, port) => versions.get(`${node}\0${port}`) ?? 0,
    takeTrace: () => [],
    takeFaults: () => [],
    streamGroups: () => groups,
    catchUp: () => {},
    sync: () => poll(),
    snapshot: () => notAttached('A snapshot'),
    restore: () => notAttached('Restoring'),
    shutdown: () => notAttached('Stopping the graph'),
  };
}

function attachedLines(
  open: () => Promise<Transport>,
  sources: Map<string, number[]>,
  told: { stop?: string },
): DebugLines {
  let stopped: LineStop | undefined;
  // The guest goes on: its next stop is a new one, even on the same line with the same values.
  const goOn = (call: (transport: Transport) => Promise<unknown>) => {
    stopped = undefined;
    told.stop = undefined;
    send(call);
  };
  let held: holds.Holding | undefined;
  const send = (call: (transport: Transport) => Promise<unknown>) => {
    void open().then(call);
  };
  return {
    get files() {
      return [...sources.keys()];
    },
    lines: (file) => sources.get(file) ?? [],
    get stopped() {
      return stopped;
    },
    setBreakpoint(file, line) {
      const actual = sources.get(file)?.find((candidate) => candidate >= line);
      if (actual !== undefined) send((transport) => lines.setBreakpoint(transport, file, actual));
      return actual;
    },
    clearBreakpoint: (file, line) =>
      send((transport) => lines.clearBreakpoint(transport, file, line)),
    pause: () => send(lines.pause),
    cancelPause: () => send(lines.cancelPause),
    holdNodes(on) {
      // Every node's next run, while a guest is stopped and the debugger goes on to the next.
      held = on ? { kind: 'all' } : undefined;
      if (held) send((transport) => holds.holdAt(transport, held as holds.Holding));
    },
    resume: () => goOn(lines.resume),
    stepIn: () => goOn(lines.stepIn),
    stepOver: () => goOn(lines.stepOver),
    stepOut: () => goOn(lines.stepOut),
    release: () => goOn(lines.release),
  };
}

function lineStop(stop: lines.LineStop): LineStop {
  return {
    reason: stop.reason,
    file: stop.file,
    line: stop.line,
    column: stop.column,
    node: stop.node,
    frames: stop.frames.map((frame) => ({
      function: frame.function,
      file: frame.file,
      line: frame.line,
    })),
    variables: stop.variables.map(variable),
    // What only stepping in a browser worker reads.
    module: -1,
    instance: -1,
    probe: -1,
    depth: -1,
  };
}

/** A variable, with its children nested again from their paths. */
function variable(from: lines.Variable): SourceVariable {
  const root: SourceVariable = {
    name: from.name,
    type: from.typeName,
    value: from.value,
    children: [],
  };
  const byPath = new Map<string, SourceVariable>([[from.name, root]]);
  for (const child of from.children) {
    const at = child.path.lastIndexOf('.');
    const parent = byPath.get(child.path.slice(0, at));
    const node: SourceVariable = {
      name: child.path.slice(at + 1),
      type: child.typeName,
      value: child.value,
      children: [],
    };
    parent?.children.push(node);
    byPath.set(child.path, node);
  }
  return root;
}
