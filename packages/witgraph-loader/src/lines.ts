// Stopping a guest on a source line: a browser lets a page set no breakpoint in
// WebAssembly, so each line has a probe (`witgraph-probes`), which reads a flag of its own
// and calls here if that is set.
//
// `LineProbes` runs on the guests' thread, and stops one by blocking that thread. Nothing
// there reads a message until it goes on, nor does a guest that spins, so all that tells it
// what to do is in shared memory, which `LineDebugger` writes from the page.

/** A source line a probe stands at. */
export interface SourceProbe {
  offset: number;
  function: number;
  file: string;
  line: number;
  column: number;
}

/** A variable, or part of one, of a guest stopped on a line. */
export interface SourceVariable {
  /** Its name: a field's, or an item's index. */
  name: string;
  /** Its type, as the source has it. */
  type: string;
  /** What it holds, on one line. */
  value: string;
  /** What it is made of, to look into. */
  children: SourceVariable[];
}

/** A function on a stopped guest's stack. */
export interface StackFrame {
  /** Its name, as the module has it. */
  function: string;
  /** The source file it is at, if it has lines that count as the guest's own. */
  file?: string;
  /** The line it is at: the call it is waiting on, unless it is the innermost. */
  line?: number;
}

/**
 * Which nodes a line breakpoint stops: two nodes of one component run the same lines. What
 * `BreakpointScope` says on wasmtime.
 */
export type BreakpointScope = { node: string } | 'every-node';

/** Whether `scope` covers a guest of `node`, if it is one's. */
function covers(scope: BreakpointScope, node: string | undefined): boolean {
  return scope === 'every-node' || scope.node === node;
}

/**
 * The breakpoints of an instance of a module: its row of the module's shared ones, which have a
 * row for each node it runs, or one for all if it runs none. Past its rows, none.
 */
function breakpointsOf(
  breakpoints: Uint8Array,
  { probes, nodes }: Pick<LineModule, 'probes' | 'nodes'>,
  instance: number,
): Uint8Array {
  const row = nodes.length === 0 ? 0 : instance;
  if (row >= Math.max(1, nodes.length)) return new Uint8Array(probes.length);
  return breakpoints.subarray(row * probes.length, (row + 1) * probes.length);
}

/** A core module with probes in it, as the graph's session knows it. */
export interface LineModule {
  probes: SourceProbe[];
  /** The node of each of its instances, in order. A module of a link has none. */
  nodes: string[];
  /** Three numbers for each call: where it is here, where in the original, and its function. */
  calls?: Uint32Array;
  /**
   * The variables at an offset in the module, of an instance stopped there: `flags` and
   * `memory` are its own, as {@link SessionLines.instance} gave them.
   */
  variablesAt?(offset: number, flags: Uint8Array, memory: Uint8Array): SourceVariable[];
}

/**
 * Which of a guest's lines count as its own, to probe, list and step through: what `OwnLines`
 * says on wasmtime. Of a file, the first that says decides: what `include` matches is, what
 * `exclude` matches is not, with `known` a library witgraph knows of is not, and with `anchor`
 * it is if it is under the guest's project, and is not otherwise. By default, a guess for any
 * language: the guest's project, less the libraries witgraph knows of.
 */
export interface OwnLinesOptions {
  /** Every line, the standard library's and dependencies' too, at about twice the cost. */
  all?: boolean;
  /**
   * Patterns of the files that are the guest's own, whatever else says: `**` is anything, `*`
   * anything but a `/`, and a pattern matches a whole path, as `/home/me/project/**`.
   */
  include?: readonly string[];
  /** Patterns of the files that are not, unless included: those of any `bindings` directory, say. */
  exclude?: readonly string[];
  /**
   * Only the files under the guest's project are its own: the directory its `run` was
   * compiled in, as its debug info says. `true` unless `false`.
   */
  anchor?: boolean;
  /**
   * The libraries witgraph knows of are not: Rust's standard library and Cargo's
   * dependencies, clang's and wasi-sdk's headers. `true` unless `false`.
   */
  known?: boolean;
}

/** Which lines are a guest's own, of options that may say more: those options alone. */
export function ownLines({
  all,
  include,
  exclude,
  anchor,
  known,
}: OwnLinesOptions): OwnLinesOptions {
  return { all, include, exclude, anchor, known };
}

/** What a session calls on the thread its guests run on. */
export interface SessionLines extends OwnLinesOptions {
  /** The modules with probes, before any is instantiated. */
  modules?(modules: readonly LineModule[]): void;
  /** The graph is being instantiated: the instances before this one are gone. */
  instantiating?(): void;
  /** An instance of a module, the flags of its probes, and its own memory, if it exports one. */
  instance?(
    module: number,
    instance: number,
    flags: ArrayBufferLike,
    memory?: WebAssembly.Memory,
  ): void;
  /** A probe whose flag is set was reached. It holds the guest for as long as it runs. */
  probe(module: number, instance: number, id: number): void;
}

/** Where a guest has stopped. */
export interface LineStop {
  /** What stopped it. */
  reason: 'breakpoint' | 'pause' | 'step';
  file: string;
  line: number;
  column: number;
  /** The node whose guest it is, if it is a node's. */
  node?: string;
  /** The functions on its stack, innermost first. */
  frames: StackFrame[];
  /**
   * The variables in scope where it stopped, innermost scope first, of the innermost function
   * only: nothing outside a function can read its locals, so those of its callers are out of
   * reach.
   */
  variables: SourceVariable[];
  /** @internal */
  module: number;
  /** @internal */
  instance: number;
  /** @internal */
  probe: number;
  /** @internal */
  depth: number;
}

/** What the page needs to control the probes of a worker's session. */
export interface LinesInfo {
  control: ArrayBufferLike;
  modules: (Pick<LineModule, 'probes' | 'nodes'> & { breakpoints: ArrayBufferLike })[];
}

/** The slots of the shared control words. */
const Control = {
  /** Stop at the next probe reached. */
  Pause: 0,
  /** How a step is to end ({@link Step}), or 0. */
  Step: 1,
  StepModule: 2,
  StepInstance: 3,
  StepProbe: 4,
  StepDepth: 5,
  /** 1 while a guest is stopped: it goes on when this is 0. */
  Stopped: 6,
  /** Hold every node's run, from the next on, however long a message would take. */
  HoldNodes: 7,
  Count: 8,
} as const;

const Step = { Into: 1, Over: 2, Out: 3 } as const;
type Step = (typeof Step)[keyof typeof Step];

/** The module's own frames on the stack: its `wasm://` address tells it from others. */
function ownFrames(): string[] {
  const limited = Error as { stackTraceLimit?: number };
  const limit = limited.stackTraceLimit;
  limited.stackTraceLimit = 1000;
  const stack = new Error().stack ?? '';
  limited.stackTraceLimit = limit;
  const frames = stack.split('\n').filter((line) => line.includes('wasm-function['));
  const address = (frame: string) => /wasm:\/\/wasm\/[0-9a-f]+/.exec(frame)?.[0];
  const own = address(frames[0] ?? '');
  return frames.filter((frame) => address(frame) === own);
}

/** The name a stack frame gives its function, or else its index. */
function frameName(frame: string): string {
  const named = /^\s*at (.+?) \(wasm:\/\//.exec(frame);
  if (named) return named[1];
  return /wasm-function\[(\d+)\]/.exec(frame)?.[1] ?? frame.trim();
}

/** A guest on its way off a line. */
interface Leaving {
  module: number;
  instance: number;
  file: string;
  line: number;
  function: number;
  depth: number;
  seen: Set<number>;
  sites: readonly number[];
  /** The function it was last seen in, and whether that is a call the line makes. */
  lastFunction: number;
  inCall: boolean;
}

/** Where a guest's lines are probed, on its thread. Blocks there, so never on a page's. */
export class LineProbes implements SessionLines {
  readonly all?: boolean;
  readonly include?: readonly string[];
  readonly exclude?: readonly string[];
  readonly anchor?: boolean;
  readonly known?: boolean;
  #control = new Int32Array(new SharedArrayBuffer(Control.Count * 4));
  #modules: readonly LineModule[] = [];
  #breakpoints: Uint8Array<SharedArrayBuffer>[] = [];
  /** By module, the functions all on one line: what a macro generated, usually. */
  #oneLine: Set<number>[] = [];
  /** By module, the probe a function starts at: before it has stored its parameters. */
  #opening: Set<number>[] = [];
  /** By module, the probes of each line: passing through them is one time running it. */
  #lineSites: Map<string, number[]>[] = [];
  /** Guests gone on from a line that would stop them again before they have left it. */
  #leaving = new Map<string, Leaving>();
  /** By module and instance, what its variables are read from. */
  #instances: { flags: Uint8Array; memory?: WebAssembly.Memory }[][] = [];
  #post: (kind: string, fields: object) => void;
  #report: () => void;

  constructor(
    post: (kind: string, fields: object) => void,
    report: () => void,
    { all, include, exclude, anchor, known }: OwnLinesOptions,
  ) {
    this.#post = post;
    this.#report = report;
    this.all = all;
    this.include = include;
    this.exclude = exclude;
    this.anchor = anchor;
    this.known = known;
  }

  /** Whether every node's run is to wait: said here, as a stopped worker reads no message. */
  get holdingNodes(): boolean {
    return Atomics.load(this.#control, Control.HoldNodes) !== 0;
  }

  info(): LinesInfo {
    return {
      control: this.#control.buffer,
      modules: this.#modules.map(({ probes, nodes }, at) => ({
        probes,
        nodes,
        breakpoints: this.#breakpoints[at].buffer,
      })),
    };
  }

  modules(modules: readonly LineModule[]): void {
    this.#modules = modules;
    this.#breakpoints = modules.map(
      ({ probes, nodes }) =>
        new Uint8Array(
          new SharedArrayBuffer(Math.max(1, probes.length * Math.max(1, nodes.length))),
        ),
    );
    this.#lineSites = modules.map(({ probes }) => {
      const sites = new Map<string, number[]>();
      for (const [id, { file, line }] of probes.entries()) {
        const key = `${file}\0${line}`;
        const known = sites.get(key);
        if (known) known.push(id);
        else sites.set(key, [id]);
      }
      return sites;
    });
    this.#opening = modules.map(({ probes }) => {
      const first = new Map<number, number>();
      for (const [id, { function: inFunction }] of probes.entries()) {
        if (!first.has(inFunction)) first.set(inFunction, id);
      }
      return new Set(first.values());
    });
    this.#oneLine = modules.map(({ probes }) => {
      const lineOf = new Map<number, number | null>();
      for (const { function: inFunction, line } of probes) {
        const seen = lineOf.get(inFunction);
        lineOf.set(inFunction, seen === undefined || seen === line ? line : null);
      }
      return new Set([...lineOf].filter(([, line]) => line !== null).map(([at]) => at));
    });
  }

  instantiating(): void {
    this.#instances = this.#modules.map(() => []);
    this.#leaving.clear();
    this.#post('lines-instantiating', {});
  }

  instance(
    module: number,
    instance: number,
    flags: ArrayBufferLike,
    memory?: WebAssembly.Memory,
  ): void {
    this.#instances[module] ??= [];
    this.#instances[module][instance] = { flags: new Uint8Array(flags), memory };
    // Set here as well, as the page may be a while seeing this, if the guest goes on to spin.
    const bytes = new Uint8Array(flags);
    if (Atomics.load(this.#control, Control.Pause)) bytes.fill(1);
    else bytes.set(this.#breakpointsOf(module, instance));
    this.#post('lines-instance', { module, instance, flags });
  }

  probe(module: number, instance: number, id: number): void {
    const control = this.#control;
    const asked = Atomics.load(control, Control.Pause) !== 0;
    const breakpoint =
      this.#breakpointsOf(module, instance)[id] === 1 && !this.#stillOnLine(module, instance, id);
    const step = this.#stepEnds(module, instance, id);
    if (!asked && !breakpoint && !step) return;
    const frames = ownFrames();
    const reason = breakpoint ? 'breakpoint' : asked ? 'pause' : 'step';
    this.#stop(reason, module, instance, id, frames);
  }

  /** Whether this guest is passing through the line it last stopped on, and not stopping again. */
  #stillOnLine(module: number, instance: number, id: number): boolean {
    const key = `${module}:${instance}`;
    const leaving = this.#leaving.get(key);
    if (!leaving) return false;
    const here = this.#modules[module].probes[id];
    if (here.file === leaving.file && here.line === leaving.line) {
      // Back at a place it has been: this is the line running again.
      if (!leaving.seen.has(id)) {
        leaving.seen.add(id);
        return true;
      }
    } else if (here.function !== leaving.function) {
      // In a call it makes, which it comes back from, or in its caller. Reading the stack is
      // slow, and a call stays in one function for most of its probes.
      if (here.function !== leaving.lastFunction) {
        leaving.lastFunction = here.function;
        leaving.inCall = ownFrames().length > leaving.depth;
      }
      if (leaving.inCall) return false;
    }
    this.#leaving.delete(key);
    this.#stopWatching(module, instance);
    return false;
  }

  /** Back to stopping only where it should, unless a pause or a step has it stopping anywhere. */
  #stopWatching(module: number, instance: number): void {
    const control = this.#control;
    if (Atomics.load(control, Control.Pause) !== 0) return;
    if (
      Atomics.load(control, Control.Step) !== 0 &&
      Atomics.load(control, Control.StepModule) === module &&
      Atomics.load(control, Control.StepInstance) === instance
    ) {
      return;
    }
    this.#instances[module]?.[instance]?.flags.set(this.#breakpointsOf(module, instance));
  }

  #breakpointsOf(module: number, instance: number): Uint8Array {
    return breakpointsOf(this.#breakpoints[module], this.#modules[module], instance);
  }

  /** Once it goes on, watches this line being left, and any other: the page has reset them. */
  #watchLeaving(module: number, instance: number, id: number, depth: number): void {
    const { file, line, function: inFunction } = this.#modules[module].probes[id];
    const key = `${module}:${instance}`;
    const sites = this.#lineSites[module].get(`${file}\0${line}`) ?? [];
    this.#leaving.delete(key);
    if (sites.length > 1) {
      this.#leaving.set(key, {
        module,
        instance,
        file,
        line,
        function: inFunction,
        depth,
        seen: new Set([id]),
        sites,
        lastFunction: inFunction,
        inCall: false,
      });
    }
    for (const [at, leaving] of this.#leaving) {
      const breakpoints = this.#breakpointsOf(leaving.module, leaving.instance);
      if (leaving.sites.some((site) => breakpoints[site] === 1)) {
        this.#instances[leaving.module]?.[leaving.instance]?.flags.fill(1);
      } else {
        this.#leaving.delete(at);
      }
    }
  }

  /** Whether a step under way ends here. Other guests go on past their own probes. */
  #stepEnds(module: number, instance: number, id: number): boolean {
    const control = this.#control;
    const step = Atomics.load(control, Control.Step);
    if (step === 0) return false;
    if (
      Atomics.load(control, Control.StepModule) !== module ||
      Atomics.load(control, Control.StepInstance) !== instance
    ) {
      return false;
    }
    const from = this.#modules[module].probes[Atomics.load(control, Control.StepProbe)];
    const here = this.#modules[module].probes[id];
    // A step does not stop in what has no lines to step through, as with what a macro made.
    if (this.#oneLine[module].has(here.function)) return false;
    // Nor on a function's opening line, where its parameters are not yet what they will be.
    if (this.#opening[module].has(id)) return false;
    const fromDepth = Atomics.load(control, Control.StepDepth);
    const depth = ownFrames().length;
    const otherLine = here.file !== from.file || here.line !== from.line;
    if (step === Step.Into) return depth > fromDepth || otherLine;
    if (step === Step.Over) return depth < fromDepth || (depth === fromDepth && otherLine);
    return depth < fromDepth;
  }

  /** A frame of the stack: `probe` is where it is, for the innermost, the one that fired. */
  #frame(module: number, frame: string, probe: number | undefined): StackFrame {
    const { probes, calls } = this.#modules[module];
    const named = { function: frameName(frame) };
    const here = probe === undefined ? this.#lineOfCall(frame, probes, calls) : probes[probe];
    return here ? { ...named, file: here.file, line: here.line } : named;
  }

  /** The line a frame waits on: the call it is at, in the original, comes after the line's start. */
  #lineOfCall(
    frame: string,
    probes: SourceProbe[],
    calls: Uint32Array | undefined,
  ): SourceProbe | undefined {
    const at = /:0x([0-9a-f]+)\)?\s*$/.exec(frame);
    if (!at || !calls) return undefined;
    const offset = Number.parseInt(at[1], 16);
    for (let call = 0; call < calls.length; call += 3) {
      if (calls[call] !== offset) continue;
      const original = calls[call + 1];
      const start = probes.findLast((candidate) => candidate.offset <= original);
      return start?.function === calls[call + 2] ? start : undefined;
    }
    return undefined;
  }

  /** Read now: stopped, this thread can be asked nothing. A failure costs only the variables. */
  #variables(module: number, instance: number, id: number): SourceVariable[] {
    const { offset } = this.#modules[module].probes[id];
    const held = this.#instances[module]?.[instance];
    if (!held?.memory) return [];
    try {
      const memory = new Uint8Array(held.memory.buffer);
      return this.#modules[module].variablesAt?.(offset, held.flags, memory) ?? [];
    } catch (error) {
      console.warn('witgraph: reading the variables of a stopped guest failed:', error);
      return [];
    }
  }

  #stop(
    reason: LineStop['reason'],
    module: number,
    instance: number,
    id: number,
    frames: string[],
  ): void {
    const control = this.#control;
    Atomics.store(control, Control.Pause, 0);
    Atomics.store(control, Control.Step, 0);
    Atomics.store(control, Control.Stopped, 1);
    const { file, line, column } = this.#modules[module].probes[id];
    const stop: LineStop = {
      reason,
      file,
      line,
      column,
      node: this.#modules[module].nodes[instance],
      frames: frames.map((frame, at) => this.#frame(module, frame, at === 0 ? id : undefined)),
      variables: this.#variables(module, instance, id),
      module,
      instance,
      probe: id,
      depth: frames.length,
    };
    // Sent now, for the page to read while this thread can send nothing.
    this.#report();
    this.#post('lines-stop', { stop });
    while (Atomics.load(control, Control.Stopped) === 1) {
      Atomics.wait(control, Control.Stopped, 1);
    }
    this.#watchLeaving(module, instance, id, frames.length);
  }
}

function askedKey(scope: BreakpointScope, file: string, line: number): string {
  return JSON.stringify([scope === 'every-node' ? null : scope.node, file, line]);
}

/** Sets breakpoints in a worker's guests, and steps through them, from the page. */
export class LineDebugger {
  #control: Int32Array;
  #modules: LinesInfo['modules'];
  #breakpoints: Uint8Array[];
  /** By module, the flags of each instance. */
  #flags: (Uint8Array | undefined)[][];
  /**
   * By file and line, where probing it stops: all of it, as an async function has a line in
   * several places, and the first by address may be taken once or never.
   */
  #sites = new Map<string, Map<number, { module: number; probe: number }[]>>();
  /** What was asked for, by its scope, file and line, with the line it came to. */
  #asked = new Map<
    string,
    { scope: BreakpointScope; file: string; line: number; actual: number }
  >();
  #stopped: LineStop | undefined;
  #onStop: (stop: LineStop) => void;

  /** @internal */
  constructor(info: LinesInfo, onStop: (stop: LineStop) => void) {
    this.#control = new Int32Array(info.control);
    this.#modules = info.modules;
    this.#breakpoints = info.modules.map((module) => new Uint8Array(module.breakpoints));
    this.#flags = info.modules.map(() => []);
    this.#onStop = onStop;
    for (const [module, { probes }] of info.modules.entries()) {
      for (const [probe, { file, line }] of probes.entries()) {
        const lines = this.#sites.get(file) ?? new Map();
        this.#sites.set(file, lines);
        const sites = lines.get(line) ?? [];
        lines.set(line, sites);
        sites.push({ module, probe });
      }
    }
  }

  /** The source files with lines to stop on, as the debug info names them. */
  get files(): string[] {
    return [...this.#sites.keys()].sort();
  }

  /**
   * Of {@link files}, those of `node`'s guest: of the core modules it runs, those of the links
   * composed into it as well. Two nodes of one component have the same.
   */
  filesOf(node: string): string[] {
    const files = new Set<string>();
    for (const { probes, nodes } of this.#modules) {
      if (nodes.includes(node)) for (const { file } of probes) files.add(file);
    }
    return [...files].sort();
  }

  /** The lines of `file` that have code, in order. */
  lines(file: string): number[] {
    return [...(this.#sites.get(file)?.keys() ?? [])].sort((a, b) => a - b);
  }

  /** Where the guest is stopped, if it is. While it is, a session answers nothing else. */
  get stopped(): LineStop | undefined {
    return this.#stopped;
  }

  /**
   * Stops the guests `scope` says on `line` of `file`, or the next line with code. Returns the
   * line it came to, or `undefined` if there is none. One set for a node and one for every node
   * are two, each cleared on its own. A running guest sees it at once, even one that is spinning.
   */
  setBreakpoint(scope: BreakpointScope, file: string, line: number): number | undefined {
    const actual = this.lines(file).find((candidate) => candidate >= line);
    if (actual === undefined) return undefined;
    this.#asked.set(askedKey(scope, file, line), { scope, file, line, actual });
    this.#mark(file, actual);
    return actual;
  }

  /** Clears what {@link setBreakpoint} set for this line in `scope`. */
  clearBreakpoint(scope: BreakpointScope, file: string, line: number): void {
    const key = askedKey(scope, file, line);
    const asked = this.#asked.get(key);
    if (!asked) return;
    this.#asked.delete(key);
    this.#mark(file, asked.actual);
  }

  /** Arms each instance's probes of a line as what is asked of it says. */
  #mark(file: string, line: number): void {
    const asked = [...this.#asked.values()].filter(
      (other) => other.file === file && other.actual === line,
    );
    for (const { module, probe } of this.#sites.get(file)?.get(line) ?? []) {
      const { nodes, probes } = this.#modules[module];
      for (let row = 0; row < Math.max(1, nodes.length); row++) {
        const value = asked.some(({ scope }) => covers(scope, nodes[row])) ? 1 : 0;
        this.#breakpoints[module][row * probes.length + probe] = value;
      }
      for (const [instance, flags] of this.#flags[module].entries()) {
        if (flags) flags[probe] = this.#breakpointsOf(module, instance)[probe];
      }
    }
  }

  #breakpointsOf(module: number, instance: number): Uint8Array {
    return breakpointsOf(this.#breakpoints[module], this.#modules[module], instance);
  }

  /** Stops at the next probe any guest reaches, even one that is spinning. */
  pause(): void {
    if (this.#stopped) return;
    Atomics.store(this.#control, Control.Pause, 1);
    for (const instances of this.#flags) {
      for (const flags of instances) flags?.fill(1);
    }
  }

  /** Takes back a {@link pause} no guest has reached yet. */
  cancelPause(): void {
    if (this.#stopped) return;
    Atomics.store(this.#control, Control.Pause, 0);
    this.#restore();
  }

  /**
   * Holds every node's run from the next on, or stops doing so. Unlike {@link RemoteSession.holdAt}
   * it reaches a stopped worker before the guest goes on, so no node can slip past.
   */
  holdNodes(on: boolean): void {
    Atomics.store(this.#control, Control.HoldNodes, on ? 1 : 0);
  }

  /** Goes on, to the next breakpoint. */
  resume(): void {
    if (!this.#stopped) return;
    this.#restore();
    this.#go();
  }

  /** Goes on to another line, in a call this one makes if it makes one. */
  stepIn(): void {
    this.#step(Step.Into);
  }

  /** Goes on to another line of this function, or of its caller if it returns. */
  stepOver(): void {
    this.#step(Step.Over);
  }

  /** Goes on until this function has returned to one with a line to stop on. */
  stepOut(): void {
    this.#step(Step.Out);
  }

  #step(how: Step): void {
    const stop = this.#stopped;
    if (!stop) return;
    this.#restore();
    const control = this.#control;
    Atomics.store(control, Control.StepModule, stop.module);
    Atomics.store(control, Control.StepInstance, stop.instance);
    Atomics.store(control, Control.StepProbe, stop.probe);
    Atomics.store(control, Control.StepDepth, stop.depth);
    Atomics.store(control, Control.Step, how);
    // Only its guest: the others go on as they would, at no cost.
    this.#flags[stop.module][stop.instance]?.fill(1);
    this.#go();
  }

  #restore(): void {
    for (const [module, instances] of this.#flags.entries()) {
      for (const [instance, flags] of instances.entries()) {
        flags?.set(this.#breakpointsOf(module, instance));
      }
    }
  }

  #go(): void {
    this.#stopped = undefined;
    Atomics.store(this.#control, Control.Stopped, 0);
    Atomics.notify(this.#control, Control.Stopped);
  }

  /** Lets go of everything: the guests run on with nothing to stop them. */
  release(): void {
    Atomics.store(this.#control, Control.Pause, 0);
    Atomics.store(this.#control, Control.Step, 0);
    this.#asked.clear();
    for (const breakpoints of this.#breakpoints) breakpoints.fill(0);
    this.#restore();
    this.#go();
  }

  /** @internal */
  // biome-ignore lint/suspicious/noExplicitAny: a message is checked by its tag.
  received(kind: string, data: any): void {
    if (kind === 'lines-instantiating') {
      this.#flags = this.#modules.map(() => []);
    } else if (kind === 'lines-instance') {
      const flags = new Uint8Array(data.flags as SharedArrayBuffer);
      this.#flags[data.module][data.instance] = flags;
      // A breakpoint set since it was made: this writes what the worker could not know.
      if (Atomics.load(this.#control, Control.Pause)) flags.fill(1);
      else flags.set(this.#breakpointsOf(data.module, data.instance));
    } else if (kind === 'lines-stop') {
      this.#stopped = data.stop;
      this.#onStop(data.stop);
    }
  }
}
