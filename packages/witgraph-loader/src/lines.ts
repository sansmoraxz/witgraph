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

/** A core module with probes in it, as the graph's session knows it. */
export interface LineModule {
  probes: SourceProbe[];
  /** The node of each of its instances, in order. A module of a link has none. */
  nodes: string[];
}

/** What a session calls on the thread its guests run on. */
export interface SessionLines {
  /** Probes reach into the standard library and dependencies too. */
  all?: boolean;
  /** The modules with probes, before any is instantiated. */
  modules?(modules: readonly LineModule[]): void;
  /** The graph is being instantiated: the instances before this one are gone. */
  instantiating?(): void;
  /** An instance of a module, and the flags of its probes. */
  instance?(module: number, instance: number, flags: ArrayBufferLike): void;
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
  /** The functions on its stack, innermost first, as the module names them. */
  frames: string[];
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
  modules: (LineModule & { breakpoints: ArrayBufferLike })[];
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
  Count: 7,
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

/** Where a guest's lines are probed, on its thread. Blocks there, so never on a page's. */
export class LineProbes implements SessionLines {
  readonly all?: boolean;
  #control = new Int32Array(new SharedArrayBuffer(Control.Count * 4));
  #modules: readonly LineModule[] = [];
  #breakpoints: Uint8Array<SharedArrayBuffer>[] = [];
  #post: (kind: string, fields: object) => void;
  #report: () => void;

  constructor(post: (kind: string, fields: object) => void, report: () => void, all?: boolean) {
    this.#post = post;
    this.#report = report;
    this.all = all;
  }

  info(): LinesInfo {
    return {
      control: this.#control.buffer,
      modules: this.#modules.map((module, at) => ({
        ...module,
        breakpoints: this.#breakpoints[at].buffer,
      })),
    };
  }

  modules(modules: readonly LineModule[]): void {
    this.#modules = modules;
    this.#breakpoints = modules.map(
      (module) => new Uint8Array(new SharedArrayBuffer(Math.max(1, module.probes.length))),
    );
  }

  instantiating(): void {
    this.#post('lines-instantiating', {});
  }

  instance(module: number, instance: number, flags: ArrayBufferLike): void {
    // Set here as well, as the page may be a while seeing this, if the guest goes on to spin.
    const bytes = new Uint8Array(flags);
    if (Atomics.load(this.#control, Control.Pause)) bytes.fill(1);
    else bytes.set(this.#breakpoints[module]);
    this.#post('lines-instance', { module, instance, flags });
  }

  probe(module: number, instance: number, id: number): void {
    const control = this.#control;
    const asked = Atomics.load(control, Control.Pause) !== 0;
    const breakpoint = this.#breakpoints[module][id] !== 0;
    const step = this.#stepEnds(module, instance, id);
    if (!asked && !breakpoint && !step) return;
    const frames = ownFrames();
    const reason = breakpoint ? 'breakpoint' : asked ? 'pause' : 'step';
    this.#stop(reason, module, instance, id, frames);
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
    const fromDepth = Atomics.load(control, Control.StepDepth);
    const depth = ownFrames().length;
    const otherLine = here.file !== from.file || here.line !== from.line;
    if (step === Step.Into) return depth > fromDepth || otherLine;
    if (step === Step.Over) return depth < fromDepth || (depth === fromDepth && otherLine);
    return depth < fromDepth;
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
      frames: frames.map(frameName),
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
  }
}

/** Sets breakpoints in a worker's guests, and steps through them, from the page. */
export class LineDebugger {
  #control: Int32Array;
  #modules: LinesInfo['modules'];
  #breakpoints: Uint8Array[];
  /** By module, the flags of each instance. */
  #flags: (Uint8Array | undefined)[][];
  /** By file and line, where probing it stops: the first site in each function. */
  #sites = new Map<string, Map<number, { module: number; probe: number }[]>>();
  /** By what was asked for, the line it came to. */
  #asked = new Map<string, Map<number, number>>();
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
      const seen = new Set<string>();
      for (const [probe, { file, line, function: inFunction }] of probes.entries()) {
        const key = `${inFunction}\0${file}\0${line}`;
        if (seen.has(key)) continue;
        seen.add(key);
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

  /** The lines of `file` that have code, in order. */
  lines(file: string): number[] {
    return [...(this.#sites.get(file)?.keys() ?? [])].sort((a, b) => a - b);
  }

  /** Where the guest is stopped, if it is. While it is, a session answers nothing else. */
  get stopped(): LineStop | undefined {
    return this.#stopped;
  }

  /**
   * Stops every guest on `line` of `file`, or the next line with code. Returns the line it
   * came to, or `undefined` if there is none. A running guest sees it at once, even one that
   * is spinning.
   */
  setBreakpoint(file: string, line: number): number | undefined {
    const actual = this.lines(file).find((candidate) => candidate >= line);
    if (actual === undefined) return undefined;
    const asked = this.#asked.get(file) ?? new Map<number, number>();
    this.#asked.set(file, asked);
    asked.set(line, actual);
    this.#mark(file, actual, 1);
    return actual;
  }

  /** Clears what {@link setBreakpoint} set for this line. */
  clearBreakpoint(file: string, line: number): void {
    const asked = this.#asked.get(file);
    const actual = asked?.get(line);
    if (actual === undefined) return;
    asked?.delete(line);
    // Another asked line may have come to the same one.
    if (![...(asked?.values() ?? [])].includes(actual)) this.#mark(file, actual, 0);
  }

  #mark(file: string, line: number, value: 0 | 1): void {
    for (const { module, probe } of this.#sites.get(file)?.get(line) ?? []) {
      this.#breakpoints[module][probe] = value;
      for (const flags of this.#flags[module]) if (flags) flags[probe] = value;
    }
  }

  /** Stops at the next probe any guest reaches, even one that is spinning. */
  pause(): void {
    if (this.#stopped) return;
    Atomics.store(this.#control, Control.Pause, 1);
    for (const instances of this.#flags) {
      for (const flags of instances) flags?.fill(1);
    }
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
      for (const flags of instances) flags?.set(this.#breakpoints[module]);
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
      else flags.set(this.#breakpoints[data.module]);
    } else if (kind === 'lines-stop') {
      this.#stopped = data.stop;
      this.#onStop(data.stop);
    }
  }
}
