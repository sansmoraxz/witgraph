import {
  AttachedSession,
  type BreakpointScope,
  type ComponentSource,
  type Contract,
  type DebugLines,
  type DebugSession,
  type Diagnostic,
  describeComponent,
  diagnosticsOf,
  endWorker,
  GraphChecker,
  init,
  type LineStop,
  type PortValues,
  RemoteSession,
  type SessionWorker,
} from 'witgraph-loader';
import { portOffset } from '../util/svg.ts';
import { weight } from '../util/weight.ts';
import { autoLayout, type LayoutEdge, type LayoutNode } from './layout.ts';
import type {
  BreakpointState,
  CapabilityInfo,
  EdgeVisual,
  EditorCommand,
  EditorLoadOptions,
  EditorMode,
  FaultReport,
  GraphJson,
  GraphJsonConnection,
  GraphJsonNode,
  GraphJsonPortRef,
  LineBreakpoint,
  NodeState,
  NodeVisual,
  OutputView,
  PortVisual,
  ResourceUse,
  SavedSnapshot,
  TraceEntry,
} from './types.ts';
import { nodeSize, nodeVisual, portVisual } from './visual.ts';

const MAX_TRACE_ENTRIES = 1000;
/**
 * The longest a tick runs before it is interrupted: work that never ends (an
 * endless stream) carries on into the next tick, and the rest of the graph
 * (a feedback loop, say) still advances a tick at a time beside it.
 */
/** How often phases are brought up to date when no tick is doing so. */
const FOLLOW_INTERVAL_MS = 100;

/** How long a stopped worker has to answer, before it is taken to be stuck and ended. */
const STOP_GRACE_MS = 2000;

/** How often phases are brought up to date during a pause. */
const PAUSED_WATCH_MS = 50;

/** How long a step waits for its node's `run`, before leaving it running. */
const STEP_GRACE_MS = 250;

/** The same, for the rest of a node stopped in its code: what is left of it can take longer. */
const NEXT_NODE_GRACE_MS = 5000;

const TICK_SLICE_MS = 100;

/** Waits for the next frame, so a run that never settles leaves the page time to draw. */
function nextFrame(): Promise<void> {
  return new Promise((resolve) =>
    typeof requestAnimationFrame === 'function'
      ? requestAnimationFrame(() => resolve())
      : setTimeout(resolve, 16),
  );
}
/** The most values an output's preview history keeps... */
const MAX_RECENT = 120;
/** ...and roughly the most bytes (a stream of video frames is heavy). */
const MAX_RECENT_BYTES = 8 * 1024 * 1024;

/** What an output carried: an {@link OutputView}, and its history's weight. */
interface Observation {
  latest: unknown;
  recent: unknown[];
  count: number;
  pending: boolean;
  /** Value and handle outputs: the version of the port's value recorded last. */
  version: number;
  /** When a value was last pulled from the port, for the preview interval. */
  pulledAt: number;
  bytes: number;
}

/** The most edits undo goes back over. */
const MAX_UNDO = 50;

/**
 * How a graph attached to is edited: by the program that runs it. Each edit is handed to it,
 * which loads the graph edited in place of its own (or refuses it, saying why), and the editor
 * attaches to that. Nothing runs in the page.
 */
export interface AttachedEditing {
  /** The program's components, in the order of the graph's component table, for the catalog. */
  components: ComponentSource[];
  /**
   * Loads `graph` in place of the graph attached to, stopped as that is (it is edited only
   * while stopped: `Debuggee::stop` in `witgraph-remote`). Resolves with where to attach to
   * it, or `null` if the graph is the same but for its layout, and is served as it was.
   */
  apply(
    graph: GraphJson,
  ): Promise<{ url: string; certificateHash: Uint8Array<ArrayBuffer> } | null>;
  /**
   * Loads the graph attached to afresh, as it is, and runs it: Restart, as for a graph run in
   * the page. Resolves with where to attach to it. Without it, an attached graph is not
   * restarted from here.
   */
  restart?(): Promise<{ url: string; certificateHash: Uint8Array<ArrayBuffer> }>;
  /**
   * Keeps `graph`, the graph running as the editor has it (its layout with it), where the
   * program keeps its graph: Save. Resolves with where to attach, or `null` if the program
   * loaded nothing again. Without it, an attached graph is not saved from here.
   */
  save?(
    graph: GraphJson,
  ): Promise<{ url: string; certificateHash: Uint8Array<ArrayBuffer> } | null>;
  /**
   * Loads the graph the program keeps in place of the one running, dropping what was changed
   * since it was saved: Revert. Resolves with where to attach.
   */
  revert?(): Promise<{ url: string; certificateHash: Uint8Array<ArrayBuffer> }>;
  /** Whether the graph running differs from the one the program keeps. */
  unsaved?(): boolean;
}

/** Whether a connection may be, and if not, why, in the compiler's words. */
export interface ConnectionCheck {
  ok: boolean;
  reason?: string;
}

/** What compiling said of connections, while the graph is as `graph` says. */
interface ConnectionChecks {
  graph: string;
  outputs: Map<string, readonly CompatibleOutput[]>;
  feedback: Map<string, ConnectionCheck>;
}

/** An output that can feed an input, as `compatibleOutputsForInput` lists them. */
export interface CompatibleOutput {
  node: string;
  port: string;
  label: string;
  self: boolean;
}

export class EditorController extends EventTarget {
  #session: DebugSession | null = null;
  /** The session, while attached to a graph a program runs elsewhere. */
  #attached: AttachedSession | null = null;
  /** What sessions run in: made as the first needs it, and ended with the controller. */
  #worker: SessionWorker | null = null;
  /** A stop being seen through, which may take a new worker. */
  #stopping: Promise<void> | null = null;
  /** Where a guest is stopped on a line. The worker answers nothing until it goes on. */
  #lineStop: LineStop | null = null;
  /** By file, the lines to stop on, kept here as each session starts with none. */
  #lineBreakpoints: LineBreakpoint[] = [];
  /** By file, its text as asked of `sources` once per load. */
  #sourceTexts = new Map<string, Promise<string | undefined>>();
  #originalOptions: EditorLoadOptions | null = null;
  /** How the graph attached to is edited, if it is. */
  #editing: AttachedEditing | null = null;

  /** The components graphs are made of: those loaded, or the program's attached to. */
  #components(): ComponentSource[] | undefined {
    return this.#originalOptions?.components ?? this.#editing?.components;
  }
  #graphJson: GraphJson | null = null;
  #dirty = false;

  #nodes = new Map<string, NodeVisual>();
  #edges: EdgeVisual[] = [];
  #mode: EditorMode = 'idle';
  #selectedNode: string | null = null;
  #selectedEdge: string | null = null;
  #traceEvents: TraceEntry[] = [];

  #breakpoints = new Set<string>();
  #stopResolve: (() => void) | null = null;
  /**
   * The nodes waiting to run, in the order they reached that point: several can at once.
   * Each is at a breakpoint, or held for the next step.
   */
  #held: (BreakpointState & { resume: () => void; breakpoint: boolean })[] = [];
  /** Whether nodes are held before they run, one step at a time, or only at breakpoints. */
  #stepping = false;
  /** The node a step let go of, until it has returned or is left running. */
  #stepRunning: string | null = null;
  #stepTimer: ReturnType<typeof setTimeout> | undefined;
  /** What `step` waits on: settled at the next pause, or when the run ends. */
  #stopWaiters: (() => void)[] = [];
  /** The run under way, of `run`, `step` or `restart`. */
  #runLoop: Promise<void> | null = null;

  #tickCount = 0;
  #previewIntervalMs = 0;
  /** Per node and output, what it carried: see {@link observe}. */
  #observed = new Map<string, Map<string, Observation>>();
  #stopRequested = false;
  #streamGroups: string[][] = [];
  /** The load warnings already in the trace. */
  #warned = new Set<string>();
  /** Whether a run or step is ticking the graph. */
  #ticking = false;
  #lastRunDurationMs: number | null = null;
  #lastRunTicks = 0;

  #undoStack: EditorCommand[] = [];
  #redoStack: EditorCommand[] = [];
  #clipboard: {
    nodes: GraphJsonNode[];
    edges: GraphJsonConnection[];
    ports: Map<string, { inputs: PortVisual[]; outputs: PortVisual[] }>;
  } | null = null;
  #compileError: string | null = null;
  /** What compiling last said of the graph, errors and warnings alike. */
  #compileDiagnostics: Diagnostic[] = [];
  /** Compiles what the graph would be, by the rules a load goes by. */
  #checker: GraphChecker | null = null;
  /** Whether phases are being kept true during a pause (`#watchWhilePaused`). */
  #watchingPause = false;
  /** Whether something is keeping phases true between ticks (`#followWhatIsRunning`). */
  #following = false;
  /**
   * What compiling said of connections, for as long as the graph stays as `graph` says.
   * Asking compiles, and the inspector asks of every port on every state change.
   */
  #checked: ConnectionChecks | null = null;
  #tickRate = 0;
  #componentContracts = new Map<number, Contract>();

  // -- Mock & capability state ------------------------------------------------
  #capabilityInfo = new Map<string, CapabilityInfo[]>();

  // -- Simulation state -------------------------------------------------------
  #savedSnapshots: SavedSnapshot[] = [];

  // -- Getters ----------------------------------------------------------------

  get mode(): EditorMode {
    return this.#mode;
  }
  get nodes(): ReadonlyMap<string, NodeVisual> {
    return this.#nodes;
  }
  get edges(): readonly EdgeVisual[] {
    return this.#edges;
  }
  get selectedNode(): string | null {
    return this.#selectedNode;
  }
  get selectedEdge(): string | null {
    return this.#selectedEdge;
  }
  get traceEvents(): readonly TraceEntry[] {
    return this.#traceEvents;
  }
  get graphJson(): object {
    return this.#graphJson ?? {};
  }
  get pausedNode(): string | null {
    return this.#held[0]?.node ?? this.#lineStop?.node ?? null;
  }
  get breakpointState(): BreakpointState | null {
    const [first] = this.#held;
    return first ? { node: first.node, inputs: first.inputs } : null;
  }
  get lastRunDurationMs(): number | null {
    return this.#lastRunDurationMs;
  }
  get compileError(): string | null {
    return this.#compileError;
  }
  /**
   * What compiling says of the graph, as it says it: errors, when it does not compile,
   * and warnings, which stop nothing, either way.
   */
  get compileDiagnostics(): readonly Diagnostic[] {
    return this.#compileDiagnostics;
  }
  /**
   * Whether what a warning asks for has been done, since compiling, which cannot see a
   * running graph: an input fed only by feedback that has a value injected.
   */
  isSettled({ code, location }: Diagnostic): boolean {
    if (code !== 'witgraph::ir::feedback_only_input' || location.port === undefined) return false;
    return this.getInjectedValue(location.nodes[0], location.port) !== undefined;
  }
  /** What compiling says about a node, or a connection of it. */
  diagnosticsForNode(id: string): readonly Diagnostic[] {
    return this.#compileDiagnostics.filter(({ location }) => {
      if (location.nodes.includes(id)) return true;
      const edge = this.#edges.find((e) => e.id === location.connection);
      return edge?.fromNode === id || edge?.toNode === id;
    });
  }
  /** What compiling says about a connection. */
  diagnosticsForConnection(id: string): readonly Diagnostic[] {
    return this.#compileDiagnostics.filter(({ location }) => location.connection === id);
  }
  get compiles(): boolean {
    return this.#compileError === null && this.#session !== null;
  }
  get lastRunTicks(): number {
    return this.#lastRunTicks;
  }
  /** The ticks made since the graph was loaded or reset. */
  get tickCount(): number {
    return this.#tickCount;
  }
  /**
   * Whether the graph can be changed: not while it runs, or is paused in a
   * run. Its edits (adding, removing, connecting, renaming, pasting, undoing,
   * mocking, importing, resetting) load the graph again, which a run is
   * using: they are refused until it stops, with a warning in the trace.
   */
  get editable(): boolean {
    // An attached graph is edited by its program, if it lets it, and only while stopped:
    // the program loads each edit in place of the graph running.
    if (this.#attached) return this.#editing !== null && this.#mode === 'idle';
    return this.#mode !== 'running' && this.#mode !== 'paused';
  }

  /** Whether this is attached to a graph a program runs elsewhere, which ticks it itself. */
  get attached(): boolean {
    return this.#attached !== null;
  }

  /**
   * The most ticks a second a run makes; `0` (the default) for as many as
   * the display draws. A tick lasts at most one interval: what is still
   * running then carries on into the next. Takes effect from the next tick,
   * so it can be changed while running.
   */
  get tickRate(): number {
    return this.#tickRate;
  }
  set tickRate(perSecond: number) {
    this.#tickRate = Number.isFinite(perSecond) && perSecond > 0 ? perSecond : 0;
  }

  /** The time between two ticks' starts, in ms: 0 when unlimited. */
  get #tickInterval(): number {
    return this.#tickRate > 0 ? 1000 / this.#tickRate : 0;
  }

  /**
   * The graph's stream groups, as it was compiled: nodes that always re-run
   * together. Read once per load, since the scheduler cannot be asked while a
   * tick holds it.
   */
  get streamGroups(): readonly string[][] {
    return this.#streamGroups;
  }
  get canUndo(): boolean {
    return this.#undoStack.length > 0;
  }
  get canRedo(): boolean {
    return this.#redoStack.length > 0;
  }
  get hasClipboard(): boolean {
    return this.#clipboard !== null;
  }
  get savedSnapshots(): readonly SavedSnapshot[] {
    return this.#savedSnapshots;
  }

  /**
   * Whether what `node`'s output `port` carries is kept: when the node
   * previews it, the node is selected (the inspector shows its outputs), or
   * it is a Value output feeding a Value input, which its node shows.
   * Nothing else is recorded or read back, so an output nobody looks at
   * costs nothing however much it carries.
   */
  isObserved(node: string, port: string): boolean {
    const visual = this.#nodes.get(node);
    if (this.#selectedNode === node || visual?.preview?.port === port) return true;
    if (visual?.outputs.find((p) => p.name === port)?.kind !== 'value') return false;
    return this.#edges.some((e) => e.fromNode === node && e.fromPort === port && !e.feedback);
  }

  /**
   * What an output has carried, as recorded so far: its latest value and those
   * before it, as a node's preview shows them. It reads nothing: new values
   * are pulled between tick slices. A Value output's latest was read from the
   * scheduler, so it is current even mid-tick.
   */
  observe(node: string, port: string): OutputView {
    const kind = this.#nodes.get(node)?.outputs.find((p) => p.name === port)?.kind ?? 'value';
    const seen = this.#observed.get(node)?.get(port);
    const view: OutputView = {
      kind,
      latest: seen?.latest,
      recent: seen?.recent ?? [],
      count: seen?.count ?? 0,
      pending: seen?.pending ?? false,
    };
    return view;
  }

  /**
   * The least time between two reads of one output for its preview, in
   * milliseconds, while a run or step is going; 0, the default, reads a new
   * value as soon as it is there. An output is read at most this often, and
   * always once more as the run ends. Reading is what a preview costs: a
   * value in a node's linear memory is copied out to be read.
   */
  get previewIntervalMs(): number {
    return this.#previewIntervalMs;
  }

  set previewIntervalMs(ms: number) {
    this.#previewIntervalMs = Math.max(0, ms);
  }

  /** Whether a run is going and `seen` was read too recently to read again. */
  #previewsLag(seen: Observation | undefined): boolean {
    return (
      this.#previewIntervalMs > 0 &&
      this.#mode === 'running' &&
      seen !== undefined &&
      performance.now() - seen.pulledAt < this.#previewIntervalMs
    );
  }

  /** What was recorded of an output last: {@link observe}'s `latest`. */
  latestOutput(node: string, port: string): unknown {
    return this.observe(node, port).latest;
  }

  /** The record of what `node`'s `port` carried, made on first use. */
  #observation(node: string, port: string): Observation {
    const ports = this.#observed.get(node) ?? new Map<string, Observation>();
    this.#observed.set(node, ports);
    let seen = ports.get(port);
    if (!seen) {
      seen = {
        latest: undefined,
        recent: [],
        count: 0,
        pending: false,
        version: 0,
        pulledAt: Number.NEGATIVE_INFINITY,
        bytes: 0,
      };
      ports.set(port, seen);
    }
    return seen;
  }

  /**
   * Records a chunk of a stream's items. Only the newest are kept, so only
   * those are recorded; the rest are counted.
   */
  #recordItems(node: string, port: string, items: ArrayLike<unknown>): void {
    const from = Math.max(0, items.length - MAX_RECENT);
    this.#observation(node, port).count += from;
    for (let i = from; i < items.length; i++) this.#record(node, port, items[i]);
  }

  /**
   * Records what the worker showed of a handle: what the output's view read, or a
   * description. No history: each would hold on to what it read.
   */
  #recordHandle(node: string, port: string, shown: unknown, count: number): void {
    const seen = this.#observation(node, port);
    seen.latest = shown;
    seen.recent = [shown];
    seen.pending = false;
    seen.count += count;
    this.#dispatch('output-changed');
  }

  /** Forgets what outputs carried. */
  #forgetObserved(node?: string): void {
    if (node === undefined) this.#observed.clear();
    else this.#observed.delete(node);
  }

  /** Records a value `node`'s `port` carried, within the history's bounds. */
  #record(node: string, port: string, value: unknown): void {
    const seen = this.#observation(node, port);
    seen.latest = value;
    seen.pending = false;
    seen.count += 1;
    seen.recent.push(value);
    seen.bytes += weight(value);
    while (
      seen.recent.length > 1 &&
      (seen.recent.length > MAX_RECENT || seen.bytes > MAX_RECENT_BYTES)
    ) {
      seen.bytes -= weight(seen.recent.shift());
    }
    this.#dispatch('output-changed');
  }

  get componentSources(): readonly {
    id: string;
    index: number;
    /** What its world's doc comment says of it. */
    docs?: string;
    inputs: PortVisual[];
    outputs: PortVisual[];
  }[] {
    if (!this.#session) return [];
    const result: {
      id: string;
      index: number;
      docs?: string;
      inputs: PortVisual[];
      outputs: PortVisual[];
    }[] = [];
    const seen = new Set<number>();

    for (const [nodeId, visual] of this.#nodes) {
      const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
      if (!graphNode) continue;
      const compIdx = this.#graphJson?.components.findIndex((c) => {
        const ref = graphNode.component as Record<string, unknown>;
        const candidate = c as Record<string, unknown>;
        return (
          EditorController.#deepEqual(candidate, ref) ||
          EditorController.#deepEqual(candidate, ref, new Set(['content_hash']))
        );
      });
      if (compIdx === undefined || compIdx < 0 || seen.has(compIdx)) continue;
      seen.add(compIdx);
      // An attached graph's components are the program's: their contracts say what they are,
      // named as a page's own are, without their content hash.
      const id =
        this.#components()?.[compIdx]?.id ?? this.#session.contracts.get(nodeId)?.id.split('#')[0];
      if (!id) continue;
      result.push({
        id,
        index: compIdx,
        docs: this.#session.contracts.get(nodeId)?.docs,
        inputs: visual.inputs,
        outputs: visual.outputs,
      });
    }

    // Also include components not yet used by any node — derive ports
    // from the component's WIT contract so the catalog and paste show them.
    const components = this.#components();
    if (!components) return result;
    for (let i = 0; i < components.length; i++) {
      if (seen.has(i)) continue;
      const comp = components[i];
      let contract = this.#componentContracts.get(i);
      if (!contract) {
        try {
          contract = describeComponent(comp.id, comp.bytes);
          this.#componentContracts.set(i, contract);
        } catch {
          /* component bytes may be invalid */
        }
      }
      result.push({
        id: comp.id,
        index: i,
        docs: contract?.docs,
        inputs: (contract?.inputs ?? []).map(portVisual),
        outputs: (contract?.outputs ?? []).map(portVisual),
      });
    }

    return result;
  }

  static #deepEqual(a: unknown, b: unknown, ignore: Set<string> = new Set()): boolean {
    if (a === b) return true;
    if (a == null || b == null) return false;
    if (typeof a !== 'object' || typeof b !== 'object') return false;
    const objA = a as Record<string, unknown>;
    const objB = b as Record<string, unknown>;
    const keysA = Object.keys(objA).filter((k) => !ignore.has(k));
    const keysB = Object.keys(objB).filter((k) => !ignore.has(k));
    if (keysA.length !== keysB.length) return false;
    return keysA.every((k) => EditorController.#deepEqual(objA[k], objB[k]));
  }

  addNodeFromCatalog(componentIndex: number, x: number, y: number): void {
    if (this.#locked()) return;
    const components = this.#components();
    if (!this.#graphJson || !components) throw new Error('no graph loaded');
    const comp = components[componentIndex];
    if (!comp) throw new Error(`no component at index ${componentIndex}`);
    const compRef = this.#graphJson.components[componentIndex];
    if (!compRef) throw new Error(`no component ref at index ${componentIndex}`);
    const id = `node-${Date.now().toString(36)}`;

    // Get port info from the catalog source (derived from existing nodes using this component)
    const source = this.componentSources.find((s) => s.index === componentIndex);
    const inputs = source?.inputs ?? [];
    const outputs = source?.outputs ?? [];

    if (!this.#graphJson) throw new Error('no graph loaded');
    const node: GraphJsonNode = {
      id,
      component: compRef,
      attrs: { 'editor.x': String(x), 'editor.y': String(y) },
    };
    this.#graphJson.nodes.push(node);
    this.#nodes.set(
      id,
      nodeVisual({
        id,
        docs: source?.docs,
        attrs: node.attrs,
        inputs,
        outputs,
        phase: 'pending',
        breakpoint: false,
      }),
    );
    this.#selectedNode = id;
    this.#selectedEdge = null;
    void this.#reloadAfterEdit();
  }

  setNodeLabel(nodeId: string, label: string): void {
    const visual = this.#nodes.get(nodeId);
    if (visual) visual.label = label;
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode) graphNode.label = label;
    this.#dispatch('state-changed');
  }

  setNodeAttr(nodeId: string, key: string, value: string): void {
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode) {
      if (!graphNode.attrs) graphNode.attrs = {};
      graphNode.attrs[key] = value;
    }
    if (key.startsWith('editor.')) this.#resize(nodeId);
    this.#dispatch('state-changed');
  }

  removeNodeAttr(nodeId: string, key: string): void {
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode?.attrs) {
      delete graphNode.attrs[key];
    }
    if (key.startsWith('editor.')) this.#resize(nodeId);
    this.#dispatch('state-changed');
  }

  /**
   * Which output a node previews: `port`, `'none'` for no preview, or
   * `null` for the first one its component declares a viewer for.
   */
  setPreview(nodeId: string, port: string | null): void {
    if (port === null) this.removeNodeAttr(nodeId, 'editor.preview');
    else this.setNodeAttr(nodeId, 'editor.preview', port);
    this.#pullSoon();
  }

  /** Resizes a node (an undoable edit); its preview region takes the height. */
  resizeNode(nodeId: string, width: number, height: number): void {
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    const before = [graphNode.attrs?.['editor.w'], graphNode.attrs?.['editor.h']] as const;
    // Only a node with a preview has a height of its own: one without
    // keeps whatever it had, for when it gets a preview back.
    const height_ = this.#nodes.get(nodeId)?.preview ? String(Math.round(height)) : before[1];
    const after = [String(Math.round(width)), height_] as const;
    if (before[0] === after[0] && before[1] === after[1]) return;
    const apply = ([w, h]: readonly [string | undefined, string | undefined]) => {
      const node = this.#graphJson?.nodes.find((n) => n.id === nodeId);
      if (!node) return;
      node.attrs ??= {};
      for (const [key, value] of [
        ['editor.w', w],
        ['editor.h', h],
      ] as const) {
        if (value === undefined) delete node.attrs[key];
        else node.attrs[key] = value;
      }
      this.#resize(nodeId);
      this.#dispatch('state-changed');
    };
    apply(after);
    this.#undoStack.push({
      description: `Resize ${nodeId}`,
      execute: () => apply(after),
      undo: () => apply(before),
    });
    if (this.#undoStack.length > MAX_UNDO) this.#undoStack.shift();
    this.#redoStack.length = 0;
  }

  /** Sizes a node's visual again from its `editor.*` attributes. */
  #resize(nodeId: string): void {
    const visual = this.#nodes.get(nodeId);
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!visual || !graphNode) return;
    const size = nodeSize(visual.inputs, visual.outputs, graphNode.attrs);
    visual.preview = undefined;
    Object.assign(visual, size);
  }

  renameNode(oldId: string, newId: string): void {
    if (this.#locked()) return;
    if (!this.#graphJson) throw new Error('no graph loaded');
    if (oldId === newId) return;
    if (this.#nodes.has(newId)) {
      this.#pushError(`Node "${newId}" already exists`);
      return;
    }
    // Update graph JSON
    const gNode = this.#graphJson.nodes.find((n) => n.id === oldId);
    if (gNode) gNode.id = newId;
    for (const conn of this.#graphJson.connections) {
      if (conn.from.node === oldId) conn.from.node = newId;
      if (conn.to.node === oldId) conn.to.node = newId;
    }
    if (this.#graphJson.links) {
      for (const link of this.#graphJson.links) {
        if (link.node === oldId) link.node = newId;
      }
    }
    // Update visual map
    const visual = this.#nodes.get(oldId);
    if (visual) {
      visual.id = newId;
      this.#nodes.delete(oldId);
      this.#nodes.set(newId, visual);
    }
    // Update edges
    for (const edge of this.#edges) {
      if (edge.fromNode === oldId) edge.fromNode = newId;
      if (edge.toNode === oldId) edge.toNode = newId;
    }
    // Update injected values
    const injected = this.#injectedValues.get(oldId);
    if (injected) {
      this.#injectedValues.delete(oldId);
      this.#injectedValues.set(newId, injected);
    }
    // A line breakpoint of the node is its still: the session is made again, and armed with it.
    for (const breakpoint of this.#lineBreakpoints) {
      if (breakpoint.scope !== 'every-node' && breakpoint.scope.node === oldId) {
        breakpoint.scope = { node: newId };
      }
    }
    // Update breakpoints
    if (this.#breakpoints.has(oldId)) {
      this.#breakpoints.delete(oldId);
      this.#breakpoints.add(newId);
      this.#sendHolds();
    }
    // Update output history and the streams' latest items
    const observed = this.#observed.get(oldId);
    if (observed) {
      this.#observed.delete(oldId);
      this.#observed.set(newId, observed);
    }
    // Update selection
    if (this.#selectedNode === oldId) this.#selectedNode = newId;
    void this.#reloadAfterEdit();
  }

  setGraphName(name: string): void {
    if (this.#graphJson) this.#graphJson.metadata.name = name;
    this.#dispatch('state-changed');
  }

  setGraphDescription(desc: string): void {
    if (this.#graphJson) this.#graphJson.metadata.description = desc || undefined;
    this.#dispatch('state-changed');
  }

  setGraphAttr(key: string, value: string): void {
    if (this.#graphJson) {
      if (!this.#graphJson.metadata.attrs) this.#graphJson.metadata.attrs = {};
      this.#graphJson.metadata.attrs[key] = value;
    }
    this.#dispatch('state-changed');
  }

  removeGraphAttr(key: string): void {
    if (this.#graphJson?.metadata.attrs) {
      delete this.#graphJson.metadata.attrs[key];
    }
    this.#dispatch('state-changed');
  }

  isPortConnected(node: string, port: string): boolean {
    return this.#edges.some((e) => e.toNode === node && e.toPort === port);
  }

  /**
   * Whether a Value input takes a value injected into it: when no connection
   * feeds it, or only one from the previous tick, which has none to give on
   * the first.
   */
  isInjectable(node: string, port: string): boolean {
    const input = this.#nodes.get(node)?.inputs.find((p) => p.name === port);
    if (input?.kind !== 'value') return false;
    return this.connectionForInput(node, port)?.feedback ?? true;
  }

  connectionForInput(node: string, port: string): EdgeVisual | undefined {
    return this.#edges.find((e) => e.toNode === node && e.toPort === port);
  }

  connectionsFromOutput(node: string, port: string): readonly EdgeVisual[] {
    return this.#edges.filter((e) => e.fromNode === node && e.fromPort === port);
  }

  availableOutputs(): readonly { node: string; port: string; kind: string; label: string }[] {
    const result: { node: string; port: string; kind: string; label: string }[] = [];
    for (const [id, n] of this.#nodes) {
      for (const p of n.outputs) {
        result.push({ node: id, port: p.name, kind: p.kind, label: `${n.label}.${p.name}` });
      }
    }
    return result;
  }

  /**
   * Whether this connection can be feedback, and if not, why: by the rules a load goes
   * by, whether it exists yet or not.
   */
  canBeFeedback(
    fromNode: string,
    fromPort: string,
    toNode: string,
    toPort: string,
  ): ConnectionCheck {
    const { feedback } = this.#currentChecks();
    const key = [fromNode, fromPort, toNode, toPort].join('\u0000');
    let check = feedback.get(key);
    if (!check) {
      const reason = this.#connectionErrors(
        { node: fromNode, port: fromPort },
        { node: toNode, port: toPort },
        true,
        true,
      ).find(({ code }) => code === 'witgraph::ir::async_feedback')?.message;
      check = reason === undefined ? { ok: true } : { ok: false, reason };
      feedback.set(key, check);
    }
    return check;
  }

  /** What compiles graphs against the loaded components, once one has loaded. */
  #graphChecker(): GraphChecker | null {
    if (this.#checker) return this.#checker;
    const components = this.#components();
    if (!components) return null;
    try {
      this.#checker = new GraphChecker(components);
    } catch {
      // The module has yet to load, or a load will say what is wrong with them.
      return null;
    }
    return this.#checker;
  }

  /**
   * What is wrong with this connection by the rules a load goes by, were the graph to
   * have it: only with the connection itself. A cycle is not, as marking a connection
   * of it feedback is the next edit, and the graph says so until then.
   */
  #connectionErrors(
    from: GraphJsonPortRef,
    to: GraphJsonPortRef,
    feedback: boolean,
    replacing: boolean,
  ): Diagnostic[] {
    const graph = this.#graphJson;
    const checker = this.#graphChecker();
    if (!graph || !checker) return [];
    const samePort = (a: GraphJsonPortRef, node?: string, port?: string) =>
      a.node === node && a.port === port;
    const others = graph.connections.filter(
      (c) => !(replacing && samePort(c.to, to.node, to.port)),
    );
    // Its id is in what is said of it, so it says what it connects.
    const name = `${from.node}.${from.port} -> ${to.node}.${to.port}`;
    let id = name;
    for (let at = 2; others.some((c) => c.id === id); at++) id = `${name} (${at})`;
    const candidate = {
      ...graph,
      connections: [...others, { id, from, to, ...(feedback ? { feedback: true } : {}) }],
    };
    let said: Diagnostic[];
    try {
      said = checker.check(JSON.stringify(candidate)) as Diagnostic[];
    } catch {
      // Nothing to say of this connection: the next load says what is wrong.
      return [];
    }
    return said.filter(({ severity, location }) => {
      if (severity !== 'error') return false;
      if (location.connection === id) return true;
      if (location.kind !== 'port') return false;
      const [node] = location.nodes;
      return samePort(from, node, location.port) || samePort(to, node, location.port);
    });
  }

  /** The first thing wrong with this connection, by `#connectionErrors`. */
  #connectionProblem(
    from: GraphJsonPortRef,
    to: GraphJsonPortRef,
    feedback: boolean,
    replacing: boolean,
  ): string | undefined {
    return this.#connectionErrors(from, to, feedback, replacing)[0]?.message;
  }

  /** What was said of connections already, of the graph as it is now. */
  #currentChecks(): ConnectionChecks {
    const graph = `${JSON.stringify(this.#graphJson)}\u0000${[...this.#nodes.keys()].join()}`;
    if (this.#checked?.graph !== graph) {
      this.#checked = { graph, outputs: new Map(), feedback: new Map() };
    }
    return this.#checked;
  }

  /**
   * Whether the graph can have this connection, and if not, why: by the rules a load
   * goes by, so what passes here does not fail for itself there.
   */
  canConnect(
    fromNode: string,
    fromPort: string,
    toNode: string,
    toPort: string,
    feedback = false,
  ): ConnectionCheck {
    const reason = this.#connectionProblem(
      { node: fromNode, port: fromPort },
      { node: toNode, port: toPort },
      feedback,
      false,
    );
    return reason === undefined ? { ok: true } : { ok: false, reason };
  }

  compatibleOutputsForInput(nodeId: string, portName: string): readonly CompatibleOutput[] {
    const { outputs: known } = this.#currentChecks();
    const key = `${nodeId}\u0000${portName}`;
    let outputs = known.get(key);
    if (!outputs) {
      outputs = this.#findCompatibleOutputs(nodeId, portName);
      known.set(key, outputs);
    }
    return outputs;
  }

  #findCompatibleOutputs(nodeId: string, portName: string): CompatibleOutput[] {
    const tgtNode = this.#nodes.get(nodeId);
    const tgtPort = tgtNode?.inputs.find((p) => p.name === portName);
    if (!tgtPort) return [];
    const to = { node: nodeId, port: portName };
    const result: CompatibleOutput[] = [];
    for (const [id, n] of this.#nodes) {
      for (const p of n.outputs) {
        if (id === nodeId && tgtPort.kind !== 'value' && tgtPort.kind !== 'handle') continue;
        // Connecting one here replaces what is connected now (`connectInput`).
        if (this.#connectionProblem({ node: id, port: p.name }, to, false, true)) continue;
        result.push({ node: id, port: p.name, label: `${n.label}.${p.name}`, self: id === nodeId });
      }
    }
    return result;
  }

  connectInput(
    nodeId: string,
    portName: string,
    fromNode: string,
    fromPort: string,
    feedback = false,
  ): void {
    // Refused, it must not cost the input what it has.
    const problem = this.#connectionProblem(
      { node: fromNode, port: fromPort },
      { node: nodeId, port: portName },
      feedback && this.canBeFeedback(fromNode, fromPort, nodeId, portName).ok,
      true,
    );
    if (problem !== undefined) {
      this.#pushError(`Cannot connect: ${problem}`);
      return;
    }
    const existing = this.connectionForInput(nodeId, portName);
    if (existing) this.removeConnection(existing.id);
    const id = `c-${fromNode}-${fromPort}-${nodeId}-${portName}`;
    this.addConnection(
      id,
      { node: fromNode, port: fromPort },
      { node: nodeId, port: portName },
      feedback,
    );
  }

  disconnectInput(nodeId: string, portName: string): void {
    const existing = this.connectionForInput(nodeId, portName);
    if (existing) this.removeConnection(existing.id);
  }

  setConnectionFeedback(connId: string, feedback: boolean): void {
    if (this.#locked()) return;
    const edge = this.#edges.find((e) => e.id === connId);
    if (!edge) return;
    const check = feedback
      ? this.canBeFeedback(edge.fromNode, edge.fromPort, edge.toNode, edge.toPort)
      : { ok: true };
    if (!check.ok) {
      this.#pushError(`Cannot make it feedback: ${check.reason}`);
      return;
    }
    edge.feedback = feedback;
    const conn = this.#graphJson?.connections.find((c) => c.id === connId);
    if (conn) conn.feedback = feedback;
    void this.#reloadAfterEdit();
  }

  // -- Resources ---------------------------------------------------------------

  /** The resources the graph declares or claims, by name. */
  get resources(): readonly ResourceUse[] {
    const graph = this.#graphJson;
    if (!graph) return [];
    const claimed = new Map<string, number>();
    for (const name of Object.keys(graph.resources ?? {})) claimed.set(name, 0);
    for (const node of graph.nodes) {
      for (const [name, { amount }] of Object.entries(node.resources ?? {})) {
        claimed.set(name, (claimed.get(name) ?? 0) + amount);
      }
    }
    return [...claimed]
      .map(([name, sum]) => {
        const declared = graph.resources?.[name];
        return {
          name,
          capacity: declared?.capacity ?? 1,
          declared: declared !== undefined,
          claimed: sum,
        };
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  /** How much a node holds of each resource while it runs. */
  claimsOf(nodeId: string): Readonly<Record<string, { amount: number }>> {
    return this.#graphJson?.nodes.find((n) => n.id === nodeId)?.resources ?? {};
  }

  /** How much of a resource a node holds while it runs. `null` claims none of it. */
  setNodeClaim(nodeId: string, resource: string, amount: number | null): void {
    if (this.#locked()) return;
    const name = this.#resourceName(resource);
    if (name === null || !this.#validResourceSize('A claim', amount)) return;
    const node = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!node) return;
    const before = node.resources?.[name]?.amount ?? null;
    if (before === amount) return;
    const apply = (value: number | null) => {
      // By id, as the node may have been renamed by the time this is undone.
      const current = this.#graphJson?.nodes.find((n) => n.id === nodeId);
      if (!current) return;
      const resources = { ...current.resources };
      if (value === null) delete resources[name];
      else resources[name] = { amount: value };
      if (Object.keys(resources).length > 0) current.resources = resources;
      else delete current.resources;
      void this.#reloadAfterEdit();
    };
    this.#applyResourceEdit(`Claim ${name} on ${nodeId}`, before, amount, apply);
  }

  /** How much there is of a resource. `null` leaves it at the default. */
  setResourceCapacity(resource: string, capacity: number | null): void {
    if (this.#locked()) return;
    const name = this.#resourceName(resource);
    if (name === null || !this.#validResourceSize('A capacity', capacity)) return;
    if (!this.#graphJson) return;
    const before = this.#graphJson.resources?.[name]?.capacity ?? null;
    if (before === capacity) return;
    const apply = (value: number | null) => {
      const graph = this.#graphJson;
      if (!graph) return;
      const resources = { ...graph.resources };
      if (value === null) delete resources[name];
      else resources[name] = { capacity: value };
      if (Object.keys(resources).length > 0) graph.resources = resources;
      else delete graph.resources;
      void this.#reloadAfterEdit();
    };
    this.#applyResourceEdit(`Capacity of ${name}`, before, capacity, apply);
  }

  #resourceName(resource: string): string | null {
    const name = resource.trim();
    if (name === '') this.#pushError('A resource needs a name');
    return name === '' ? null : name;
  }

  #validResourceSize(what: string, size: number | null): boolean {
    if (size === null || (Number.isFinite(size) && size > 0)) return true;
    this.#pushError(`${what} must be above 0`);
    return false;
  }

  #applyResourceEdit(
    description: string,
    before: number | null,
    after: number | null,
    apply: (value: number | null) => void,
  ): void {
    apply(after);
    this.#undoStack.push({
      description,
      execute: () => apply(after),
      undo: () => apply(before),
    });
    if (this.#undoStack.length > MAX_UNDO) this.#undoStack.shift();
    this.#redoStack.length = 0;
    this.#dispatch('state-changed');
  }

  // -- Import / Load -----------------------------------------------------------

  async importGraph(json: string): Promise<void> {
    if (this.#locked()) return;
    if (!this.#originalOptions) throw new Error('no session loaded');
    const graphJson = JSON.parse(json) as GraphJson;
    this.#graphJson = graphJson;
    this.#undoStack = [];
    this.#redoStack = [];
    this.#traceEvents = [];
    this.#forgetObserved();
    this.#tickCount = 0;
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#selectedNode = null;
    this.#selectedEdge = null;
    await this.#reloadAfterEdit();
  }

  async importBundle(json: string): Promise<void> {
    if (this.#locked()) return;
    const prev = this.#originalOptions;
    if (!prev) throw new Error('no session loaded');
    const bundle = JSON.parse(json) as {
      version: number;
      graph: GraphJson;
      components: { id: string; wit?: string; bytes: string }[];
    };
    const components = bundle.components.map((c) => {
      const binary = atob(c.bytes);
      const bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
      return { id: c.id, bytes, wit: c.wit } as import('witgraph-loader').ComponentSource;
    });
    this.dispose();
    await this.load({
      graph: bundle.graph,
      components,
      wasm: prev.wasm,
      worker: prev.worker,
      mocks: prev.mocks,
    });
  }

  async importGraphFile(): Promise<void> {
    if (this.#locked()) return;
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.json';
    return new Promise((resolve) => {
      input.addEventListener('change', async () => {
        const file = input.files?.[0];
        if (!file) {
          resolve();
          return;
        }
        try {
          const text = await file.text();
          const parsed = JSON.parse(text);
          if (parsed.version && parsed.graph && parsed.components) {
            await this.importBundle(text);
          } else {
            await this.importGraph(text);
          }
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          this.#pushError(`Import failed: ${msg}`);
        }
        resolve();
      });
      input.click();
    });
  }

  // -- Session lifecycle ------------------------------------------------------

  /**
   * Attaches to a graph a program runs on wasmtime and serves with `witgraph-remote`, at the
   * URL and certificate hash it printed. The program ticks it: from here, nodes are held,
   * guests stopped on lines, outputs read and inputs written, and the graph is not changed.
   */
  async attach(options: {
    url: string;
    certificateHash: Uint8Array<ArrayBuffer>;
    /** Lets the graph be edited, by the program: without, it is read as it is. */
    editing?: AttachedEditing;
  }): Promise<void> {
    this.#session?.dispose();
    this.#session = null;
    this.#attached = null;
    this.#endWorker();
    this.#forgetChecker();
    this.#originalOptions = null;
    this.#editing = options.editing ?? null;
    let session: AttachedSession | null = null;
    session = await AttachedSession.attach({
      ...options,
      beforeRun: (node, held) => this.#beforeRun(session, node, held),
      onStop: (stop: LineStop): void => {
        if (this.#session === session) this.#stopOnLine(stop);
      },
      onState: (): void => {
        if (this.#session !== session) return;
        // Stopped or run, by this debugger or by one that went.
        if (session && !this.#lineStop && this.#held.length === 0) {
          this.#mode = session.running ? 'running' : 'idle';
        }
        this.#pollState();
        void this.#pullObserved(false);
        this.#dispatch('state-changed');
      },
      onError: (message: string): void => {
        if (this.#session === session) this.#warn(message);
      },
    });
    this.#attached = session;
    this.#session = session;
    this.#graphJson = JSON.parse(session.described) as GraphJson;
    this.#adoptInjections(this.#graphJson);
    this.#streamGroups = session.graph.streamGroups();
    this.#compileDiagnostics = [];
    this.#compileError = null;
    this.#dirty = false;
    this.#buildVisuals();
    this.#observedSent = '';
    this.#armLineBreakpoints();
    this.#sendHolds();
    this.#collectCapabilities();
    // Ticking it, unless a debugger stopped it.
    this.#mode = session.running ? 'running' : 'idle';
    this.#dispatch('state-changed');
  }

  /** Says `message` in the trace, as a warning. */
  #warn(message: string): void {
    this.#traceEvents.push({ event: 'warning', message, timestamp: performance.now() });
    if (this.#traceEvents.length > MAX_TRACE_ENTRIES) {
      this.#traceEvents = this.#traceEvents.slice(-MAX_TRACE_ENTRIES);
    }
    this.#dispatch('trace-event');
  }

  async load(options: EditorLoadOptions): Promise<void> {
    this.#attached = null;
    if (this.#originalOptions?.worker !== options.worker) this.#endWorker();
    this.#originalOptions = options;
    this.#forgetChecker();
    const graphJson: GraphJson =
      typeof options.graph === 'string'
        ? (JSON.parse(options.graph) as GraphJson)
        : structuredClone(options.graph);
    this.#graphJson = graphJson;
    // The checker and the catalog read components on this thread, the session in its worker.
    await init(options.wasm);
    await this.#createSession(graphJson);
    this.#compileError = null;
    this.#mode = 'idle';
    this.#dispatch('state-changed');
  }

  /** Lets the checker go, and what it said: it knows the components of one load. */
  #forgetChecker(): void {
    this.#checker?.free();
    this.#checker = null;
    this.#checked = null;
  }

  /** Ends the worker, if it can be: one that cannot is used again. */
  #endWorker(): void {
    if (this.#worker && endWorker(this.#worker)) this.#worker = null;
  }

  dispose(): void {
    this.#forgetChecker();
    this.#session?.dispose();
    this.#session = null;
    this.#endWorker();
    this.#lineStop = null;
    this.#lineBreakpoints = [];
    this.#sourceTexts.clear();
    this.#streamGroups = [];
    this.#warned.clear();
    this.#nodes.clear();
    this.#edges = [];
    this.#mode = 'idle';
    this.#selectedNode = null;
    this.#selectedEdge = null;
    this.#traceEvents = [];
    this.#graphJson = null;
    this.#dirty = false;
    this.#stopResolve = null;
    this.#held = [];
    this.#endStepping();
    this.#tickCount = 0;
    this.#forgetObserved();
    this.#injectedValues.clear();
    this.#undoStack.length = 0;
    this.#redoStack.length = 0;
    this.#clipboard = null;
    this.#capabilityInfo.clear();
    this.#savedSnapshots = [];
    this.#compileError = null;
    this.#compileDiagnostics = [];
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#componentContracts.clear();
  }

  async reset(): Promise<void> {
    if (this.#locked()) return;
    if (!this.#originalOptions) return;
    const emptyGraph: GraphJson = {
      metadata: {
        name: this.#graphJson?.metadata.name ?? 'untitled',
        description: this.#graphJson?.metadata.description,
      },
      components: this.#graphJson?.components ?? [],
      nodes: [],
      connections: [],
    };
    this.dispose();
    await this.load({ ...this.#originalOptions!, graph: emptyGraph });
  }

  // -- Execution --------------------------------------------------------------

  /** Runs until a breakpoint, or until the graph settles or is stopped. Paused, continues. */
  async run(): Promise<void> {
    if (this.#mode === 'paused') {
      this.resumeBreakpoint();
      return;
    }
    if (this.#mode === 'running') return;
    // The program running an attached graph ticks it: a debugger that stopped it lets it go on,
    // with its breakpoints set again, as a run here starts.
    if (this.#attached) {
      this.#armLineBreakpoints();
      this.#sendHolds();
      this.#mode = 'running';
      this.#dispatch('state-changed');
      await this.#attached.run();
      return;
    }
    this.#setStepping(false);
    await this.#startRun('run');
    await this.#runLoop;
  }

  /**
   * Runs one node, and pauses before the next. From a stopped graph this pauses before its
   * first; past the last node, the next step starts over from the top. Resolves at the
   * next pause, or once the run is over.
   */
  async step(): Promise<void> {
    if (this.#mode === 'running') return;
    if (this.#lineStop) {
      // Stopped in a node's code: the rest of that node is the node to run.
      this.#stepRunning = this.#lineStop.node ?? null;
      this.#setStepping(true);
      const stopped = this.#nextStop();
      this.#goOnFromLine((lines) => lines.resume());
      if (this.#stepRunning !== null) {
        const node = this.#stepRunning;
        clearTimeout(this.#stepTimer);
        this.#stepTimer = setTimeout(() => this.#stepReturned(node), NEXT_NODE_GRACE_MS);
      }
      await stopped;
      return;
    }
    if (this.#mode === 'paused') {
      const held = this.#held.shift();
      if (!held) return;
      this.#setStepping(true);
      this.#stepRunning = held.node;
      this.#mode = 'running';
      // A run that never returns, reading an endless stream, must not end the stepping.
      clearTimeout(this.#stepTimer);
      this.#stepTimer = setTimeout(() => this.#stepReturned(held.node), STEP_GRACE_MS);
      const stopped = this.#nextStop();
      held.resume();
      this.#dispatch('state-changed');
      await stopped;
      return;
    }
    this.#setStepping(true);
    const stopped = this.#nextStop();
    await this.#startRun('step');
    await stopped;
  }

  /** Goes on from a line to the next in this function, or its caller if it returns. */
  async stepOver(): Promise<void> {
    if (this.#lineStop) return this.#stepLine((lines) => lines.stepOver());
  }

  /** Goes on from a line: into a call, if the line makes one, and stops at the next line. */
  async stepIn(): Promise<void> {
    if (this.#lineStop) return this.#stepLine((lines) => lines.stepIn());
    // From a node about to run, into its code.
    if (this.#mode !== 'paused' || this.#held.length === 0) return;
    this.#session?.lines?.pause();
    return this.step();
  }

  /** Goes on from a line until its function has returned. */
  async stepOut(): Promise<void> {
    if (this.#lineStop) return this.#stepLine((lines) => lines.stepOut());
  }

  /** Lets go of the guest stopped on a line, and settles at its next stop. */
  async #stepLine(go: (lines: DebugLines) => void): Promise<void> {
    const stopped = this.#nextStop();
    this.#goOnFromLine(go);
    // With no run to end it, a step that finds no line has nothing else to settle it.
    if (!this.#runLoop && !this.#ticking) this.#settleStopWaiters();
    await stopped;
  }

  #goOnFromLine(go: (lines: DebugLines) => void): void {
    const lines = this.#session?.lines;
    this.#lineStop = null;
    this.#mode = this.#runLoop || this.#ticking || this.#attached ? 'running' : 'idle';
    if (lines) go(lines);
    this.#dispatch('state-changed');
  }

  /** Whether any guest of the graph can be stopped on a line. */
  get lineFiles(): readonly string[] {
    return this.#session?.lines?.files ?? [];
  }

  /**
   * Of {@link lineFiles}, those of `node`'s guest, links composed into it included. Two nodes of
   * one component have the same.
   */
  lineFilesOf(node: string): readonly string[] {
    return this.#session?.lines?.filesOf(node) ?? [];
  }

  /** The lines of `file` with code. */
  linesOf(file: string): readonly number[] {
    return this.#session?.lines?.lines(file) ?? [];
  }

  /** Where a guest is stopped on a line, if one is. */
  get lineStop(): LineStop | null {
    return this.#lineStop;
  }

  /** The line breakpoints set in `file`, one at most on a line: of a node, or of every node. */
  lineBreakpoints(file: string): readonly LineBreakpoint[] {
    return this.#lineBreakpoints
      .filter((breakpoint) => breakpoint.file === file)
      .map((breakpoint) => ({ ...breakpoint }));
  }

  /** The breakpoint on `line` of `file`, or the next line with code, if one is set. */
  lineBreakpointAt(file: string, line: number): LineBreakpoint | undefined {
    const actual = this.linesOf(file).find((candidate) => candidate >= line);
    const found = this.#lineBreakpoints.find(
      (breakpoint) => breakpoint.file === file && breakpoint.line === actual,
    );
    return found && { ...found };
  }

  /**
   * As a click in a debugger's gutter: takes away the breakpoint on `line` of `file`, or the next
   * line with code, whichever nodes it stops, or with none there sets one that stops every node.
   */
  /**
   * What a click on a line in the Source panel sets: a breakpoint in every node that runs the
   * line (`'every-node'`, the default, as in most debuggers), or only in the node whose source
   * is on show (`'node'`). The panel's menu offers the other either way.
   */
  breakpointClickScope: 'every-node' | 'node' = 'every-node';

  toggleLineBreakpoint(file: string, line: number): void {
    this.setLineBreakpoint(file, line, this.lineBreakpointAt(file, line) ? null : 'every-node');
  }

  /**
   * Puts a breakpoint on `line` of `file`, or the next line with code, stopping the nodes `scope`
   * says, in place of any there: with `null`, takes it away. A guest that is running sees it at
   * once.
   */
  setLineBreakpoint(file: string, line: number, scope: BreakpointScope | null): void {
    const lines = this.#session?.lines;
    const actual = this.linesOf(file).find((candidate) => candidate >= line);
    if (!lines || actual === undefined) return;
    const at = this.#lineBreakpoints.findIndex(
      (breakpoint) => breakpoint.file === file && breakpoint.line === actual,
    );
    if (at >= 0) {
      lines.clearBreakpoint(this.#lineBreakpoints[at].scope, file, actual);
      this.#lineBreakpoints.splice(at, 1);
    }
    if (scope !== null) {
      this.#lineBreakpoints.push({ scope, file, line: actual });
      lines.setBreakpoint(scope, file, actual);
    }
    this.#dispatch('state-changed');
  }

  /** Gives the session the breakpoints: each starts with none, and a stop clears them. */
  #armLineBreakpoints(): void {
    const lines = this.#session?.lines;
    if (!lines) return;
    for (const { scope, file, line } of this.#lineBreakpoints)
      lines.setBreakpoint(scope, file, line);
  }

  /** The text of a source file, if the host can say. */
  sourceOf(file: string): Promise<string | undefined> {
    let text = this.#sourceTexts.get(file);
    if (!text) {
      const asked = this.#originalOptions?.sources;
      const attached = this.#attached;
      text = Promise.resolve(attached ? attached.source(file) : asked?.(file)).catch(
        () => undefined,
      );
      this.#sourceTexts.set(file, text);
    }
    return text;
  }

  /** Stops holding anything but breakpoints from the next node on, and goes on. */
  resumeBreakpoint(): void {
    if (this.#mode !== 'paused') return;
    this.#setStepping(false);
    if (this.#lineStop) {
      this.#goOnFromLine((lines) => lines.resume());
      this.#showStop();
      return;
    }
    this.#stepRunning = null;
    clearTimeout(this.#stepTimer);
    // The node paused at goes on, as does any only held for a step. Another at a breakpoint
    // of its own is the next stop.
    const [first, ...rest] = this.#held;
    const staying = rest.filter((held) => held.breakpoint);
    const going = [first, ...rest.filter((held) => !held.breakpoint)];
    this.#held = staying;
    for (const held of going) held?.resume();
    this.#mode = staying.length > 0 ? 'paused' : 'running';
    this.#showStop();
    this.#dispatch('state-changed');
  }

  /** Holds each node from the next on, as a step would. What is running goes on. */
  pause(): void {
    if (this.#mode !== 'running' || (!this.#runLoop && !this.#attached)) return;
    this.#setStepping(true);
    this.#mode = 'paused';
    // Or at the next line of what is running, even if it is spinning.
    this.#session?.lines?.pause();
    this.#showStop();
    this.#dispatch('state-changed');
  }

  /** Whether {@link restart} can: always, but for an attached graph its program cannot restart. */
  get restartable(): boolean {
    return !this.#attached || this.#editing?.restart !== undefined;
  }

  /**
   * Stops what is running and runs the graph again from the top, keeping its inputs. An
   * attached graph its program restarts: loaded afresh, and run.
   */
  async restart(): Promise<void> {
    if (this.#attached) {
      const restart = this.#editing?.restart;
      if (!restart) return;
      await this.#changeAttached('restart', () => restart(), true);
      return;
    }
    await this.stop();
    await this.#runLoop;
    await this.run();
  }

  /** Whether an attached graph is saved from here, by its program: Save and Revert. */
  get savable(): boolean {
    return this.#attached !== null && this.#editing?.save !== undefined;
  }

  /** Whether an attached graph differs from the one its program keeps. */
  get unsaved(): boolean {
    return this.#editing?.unsaved?.() ?? false;
  }

  /** Has the program attached to keep the graph running, as here (its layout with it). */
  async saveAttached(): Promise<void> {
    const save = this.#editing?.save;
    const graph = this.#graphJson;
    if (!this.#attached || !save || !graph) return;
    await this.#changeAttached('save', () => save(structuredClone(graph)), true);
  }

  /**
   * Has the program attached to load the graph it keeps in place of the one running, dropping
   * what was changed since: only while stopped, as an edit.
   */
  async revertAttached(): Promise<void> {
    const revert = this.#editing?.revert;
    if (!this.#attached || !revert || this.#locked()) return;
    this.#undoStack = [];
    this.#redoStack = [];
    await this.#changeAttached('revert', () => revert(), false);
  }

  /**
   * Asks the program attached to for `change`, and attaches to what it serves then, if it
   * loaded the graph again: with the layout the editor had, if `keepLayout`.
   */
  async #changeAttached(
    what: string,
    change: () => Promise<{ url: string; certificateHash: Uint8Array<ArrayBuffer> } | null>,
    keepLayout: boolean,
  ): Promise<void> {
    const editing = this.#editing;
    if (!editing) return;
    if (this.#reloading) {
      this.#warn(`The graph is being changed: ${what} it once that is done`);
      return;
    }
    this.#reloading = true;
    try {
      const draft = this.#graphJson;
      const selected = this.#selectedNode;
      const next = await change();
      if (next) {
        await this.attach({ ...next, editing });
        // Drawn where the editor had them.
        if (keepLayout && draft) {
          this.#graphJson = draft;
          this.#buildVisuals();
        }
        if (selected && this.#nodes.has(selected)) this.#selectedNode = selected;
      }
    } catch (err) {
      this.#pushError(`Cannot ${what}: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      this.#reloading = false;
    }
    this.#dispatch('state-changed');
  }

  /** One tick, at most a slice long, whatever runs in it: no node is held for a step. */
  async tick(): Promise<void> {
    if (this.#mode === 'running' || this.#mode === 'paused') return;
    await this.#stopping;
    try {
      if (this.#dirty || !this.#session) await this.#reload();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`Cannot step: ${msg}`);
      this.#mode = 'error';
      this.#dispatch('state-changed');
      return;
    }
    if (!this.#session) {
      this.#pushError('Cannot step: graph does not compile');
      this.#mode = 'error';
      this.#dispatch('state-changed');
      return;
    }

    this.#mode = 'running';
    this.#stopRequested = false;
    await this.#rerunCancelled();
    this.#dispatch('state-changed');

    const startTime = performance.now();
    try {
      // What is still running carries on until the next tick, or until Stop.
      if ((await this.#tick()) === 'aborted') this.#mode = 'error';
    } catch (err) {
      this.#mode = 'error';
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`Step failed: ${msg}`);
    }
    await this.#pullObserved(true);
    this.#lastRunDurationMs = performance.now() - startTime;
    this.#lastRunTicks = 1;

    const mode = this.#mode as EditorMode;
    if (mode === 'running' || mode === 'paused') this.#mode = 'idle';
    this.#settleStopWaiters();
    this.#dispatch('state-changed');
    this.#followWhatIsRunning();
  }

  /** A guest has stopped on a line: shows where. */
  #stopOnLine(stop: LineStop): void {
    clearTimeout(this.#stepTimer);
    this.#stepRunning = null;
    this.#lineStop = stop;
    this.#mode = 'paused';
    if (stop.node) {
      this.#selectedNode = stop.node;
      this.#selectedEdge = null;
    }
    this.#pollState();
    this.#settleStopWaiters();
    this.#dispatch('state-changed');
    this.#watchWhilePaused();
  }

  #warnOfNoLines(): void {
    const message =
      'Guests cannot be stopped on source lines here: that needs shared memory, so a page ' +
      'served with Cross-Origin-Opener-Policy: same-origin and ' +
      'Cross-Origin-Embedder-Policy: require-corp';
    if (this.#warned.has(message)) return;
    this.#warned.add(message);
    this.#traceEvents.push({ event: 'warning', message, timestamp: performance.now() });
    this.#dispatch('trace-event');
  }

  /** Whether each node is held before it runs, or only those with a breakpoint. */
  #setStepping(on: boolean): void {
    this.#stepping = on;
    this.#sendHolds();
    // Directly too: a worker stopped on a line reads no message.
    this.#session?.lines?.holdNodes(on);
  }

  /** Tells the worker whose runs wait for this thread: the others do not. */
  #sendHolds(): void {
    this.#session?.holdAt(this.#holds());
  }

  #holds(): string[] | 'all' {
    return this.#stepping ? 'all' : [...this.#breakpoints];
  }

  /** The stream and future outputs looked at last told to the worker. */
  #observedSent = '';

  /** Tells the worker which outputs to send what they carry: nothing else crosses. */
  #sendObserved(): void {
    const session = this.#session;
    if (!session) return;
    const ports: [string, string][] = [];
    for (const [id, visual] of this.#nodes) {
      for (const port of visual.outputs) {
        if (port.kind !== 'stream' && port.kind !== 'future') continue;
        if (this.isObserved(id, port.name)) ports.push([id, port.name]);
      }
    }
    const sent = JSON.stringify(ports);
    if (sent === this.#observedSent) return;
    this.#observedSent = sent;
    session.observeOnly(ports);
  }

  /** Starts the run loop, once the graph is loaded. */
  async #startRun(what: 'run' | 'step'): Promise<void> {
    const name = what === 'run' ? 'run' : 'step';
    await this.#stopping;
    try {
      if (this.#dirty || !this.#session) await this.#reload();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`Cannot ${name}: ${msg}`);
      this.#mode = 'error';
      this.#endStepping();
      this.#dispatch('state-changed');
      return;
    }
    if (!this.#session) {
      this.#pushError(`Cannot ${name}: graph does not compile`);
      this.#mode = 'error';
      this.#endStepping();
      this.#dispatch('state-changed');
      return;
    }

    this.#mode = 'running';
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#stopRequested = false;
    await this.#rerunCancelled();
    this.#dispatch('state-changed');
    this.#runLoop = this.#runTicks(name).finally(() => {
      this.#runLoop = null;
    });
  }

  async #runTicks(name: string): Promise<void> {
    const startTime = performance.now();
    let ticks = 0;
    try {
      // Until the graph settles or is stopped: a graph with an endless
      // stream or a feedback loop runs until it is stopped.
      for (;;) {
        const started = performance.now();
        const result = await this.#tick();
        ticks++;
        if (result === 'idle' && ticks === 1 && !this.#stopRequested) {
          // Nothing was owed, so the graph had finished: this starts it again, as running
          // a finished program would, or breakpoints and steps would have nothing to stop at.
          await this.#rerunAll();
          continue;
        }
        if (result === 'idle' || result === 'stopped') break;
        if (result === 'aborted') {
          this.#mode = 'error';
          break;
        }
        // The next tick starts a frame later at the soonest, and no sooner
        // than the tick rate allows (read each frame: it may change).
        do await nextFrame();
        while (!this.#stopRequested && performance.now() - started < this.#tickInterval);
        // Stopped between two ticks: `stop` already shut the graph down.
        if (this.#stopRequested) break;
      }
    } catch (err) {
      this.#mode = 'error';
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`${name === 'run' ? 'Run' : 'Step'} failed: ${msg}`);
    } finally {
      const mode = this.#mode as EditorMode;
      if (mode === 'running' || mode === 'paused') this.#mode = 'idle';
      this.#endStepping();
      await this.#pullObserved(true);
      this.#lastRunDurationMs = performance.now() - startTime;
      this.#lastRunTicks = ticks;
      this.#dispatch('state-changed');
    }
  }

  /** Every node owes a run, on the inputs it has. A node the graph has no longer is no matter. */
  async #rerunAll(): Promise<void> {
    const graph = this.#session?.graph;
    if (!graph) return;
    await Promise.all([...this.#nodes.keys()].map((id) => graph.rerun(id).catch(() => {})));
  }

  /** Settled at the next pause, or when the run is over. */
  #nextStop(): Promise<void> {
    return new Promise((resolve) => this.#stopWaiters.push(resolve));
  }

  #settleStopWaiters(): void {
    for (const resolve of this.#stopWaiters.splice(0)) resolve();
  }

  #endStepping(): void {
    this.#setStepping(false);
    this.#stepRunning = null;
    clearTimeout(this.#stepTimer);
    this.#settleStopWaiters();
  }

  /** The node a step let go of has returned, or is left running. */
  #stepReturned(node: string): void {
    if (this.#stepRunning !== node) return;
    this.#stepRunning = null;
    clearTimeout(this.#stepTimer);
    this.#showStop();
    // The scheduler starts what comes next just after this. If nothing does, as with only
    // streams left running, the step is over all the same.
    if (this.#held.length === 0) {
      this.#pollState();
      this.#dispatch('state-changed');
      this.#stepTimer = setTimeout(() => this.#settleStopWaiters(), STEP_GRACE_MS);
    }
  }

  /** If a node waits to run and no step is under way, pauses there, and shows it. */
  #showStop(): void {
    if (this.#stepRunning !== null) return;
    const [first] = this.#held;
    if (!first) return;
    // Stopped here first: a pause asked for has what it asked for.
    this.#session?.lines?.cancelPause();
    clearTimeout(this.#stepTimer);
    this.#mode = 'paused';
    this.#selectedNode = first.node;
    this.#selectedEdge = null;
    this.#pollState();
    this.#pullSoon();
    this.#settleStopWaiters();
    this.#dispatch('state-changed');
    this.#watchWhilePaused();
  }

  /**
   * Paused, a tick is held open, and no tick ending brings phases up to date, though what is
   * not held goes on and finishes: keeps them true until the pause is over.
   */
  #watchWhilePaused(): void {
    const session = this.#session;
    if (this.#watchingPause || !session) return;
    this.#watchingPause = true;
    const watch = () => {
      if (session !== this.#session || this.#mode !== 'paused') {
        this.#watchingPause = false;
        return;
      }
      const phases = () => [...this.#nodes.values()].map((node) => node.phase).join();
      const before = phases();
      this.#pollState();
      if (phases() !== before) {
        this.#pullSoon();
        this.#dispatch('state-changed');
      }
      const timer = setTimeout(watch, PAUSED_WATCH_MS) as { unref?: () => void };
      timer.unref?.();
    };
    const timer = setTimeout(watch, 0) as { unref?: () => void };
    timer.unref?.();
  }

  /**
   * After a step, what it left running carries on with no tick to report it: keeps node
   * phases true until it has ended, or the next tick takes over.
   */
  #followWhatIsRunning(): void {
    const session = this.#session;
    if (this.#following || !session) return;
    const running = () => [...this.#nodes.values()].some((node) => node.phase === 'running');
    if (!running()) return;
    this.#following = true;
    const follow = () => {
      if (session !== this.#session || this.#mode !== 'idle') {
        this.#following = false;
        return;
      }
      if (!this.#ticking) {
        const before = [...this.#nodes.values()].map((node) => node.phase).join();
        try {
          session.graph.catchUp();
        } catch {}
        this.#pollState();
        if ([...this.#nodes.values()].map((node) => node.phase).join() !== before) {
          void this.#pullObserved(true);
          this.#dispatch('state-changed');
        }
        if (!running()) {
          this.#following = false;
          return;
        }
      }
      const timer = setTimeout(follow, FOLLOW_INTERVAL_MS) as { unref?: () => void };
      // (a graph left running should not keep a process alive for this)
      timer.unref?.();
    };
    follow();
  }

  /**
   * Runs one tick, interrupted after {@link TICK_SLICE_MS} (or the tick
   * interval, if shorter) or by `stop`,
   * whichever comes first; a stop also shuts the graph down. A node held at
   * a breakpoint holds no slice: its tick waits for it.
   */
  async #tick(): Promise<'idle' | 'progress' | 'aborted' | 'stopped'> {
    const session = this.#session;
    if (!session) throw new Error('no session loaded');
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stopped = new Promise<void>((resolve) => {
      this.#stopResolve = resolve;
    });
    // A tick fits in its interval, so the rate holds beside endless work.
    const sliceMs =
      this.#tickInterval > 0 ? Math.min(TICK_SLICE_MS, this.#tickInterval) : TICK_SLICE_MS;
    const slice = new Promise<void>((resolve) => {
      timer = setTimeout(resolve, sliceMs);
    });
    // Told first, so that the tick's first runs are seen.
    this.#sendObserved();
    this.#ticking = true;
    let tick: Awaited<ReturnType<DebugSession['tick']>>;
    try {
      // Paused at a breakpoint, a tick ends only when resumed or stopped.
      const stop = Promise.race([stopped, slice.then(() => this.#paused())]);
      tick = await session.tick(stop);
    } catch (error) {
      // Its worker was ended, for not stopping.
      if (this.#session !== session) return 'stopped';
      throw error;
    } finally {
      this.#ticking = false;
      this.#stopResolve = null;
      clearTimeout(timer);
    }
    this.#tickCount++;
    const stop = this.#stopRequested;
    if (stop) await this.#shutdown();
    this.#pollState();
    await this.#pullObserved(stop);
    if (stop) return 'stopped';
    // Only `fatal` ends a run: past any other fault, ticking rebuilds what is owed.
    if (tick.result === 'faulted') return tick.fault.kind === 'fatal' ? 'aborted' : 'progress';
    return tick.result === 'idle' ? 'idle' : 'progress';
  }

  /** Resolves once no node is held at a breakpoint. */
  async #paused(): Promise<void> {
    while ((this.#held.length > 0 || this.#lineStop) && !this.#stopRequested) await nextFrame();
  }

  /** Stops the graph: what is still running ends, streams included. */
  async #shutdown(): Promise<void> {
    // A worker ended for not stopping takes it with it.
    await this.#session?.graph.shutdown().catch(() => {});
  }

  /** Nodes a stop cancelled run again on their latched inputs. */
  async #rerunCancelled(): Promise<void> {
    const graph = this.#session?.graph;
    if (!graph) return;
    const cancelled = [...this.#nodes].filter(([, visual]) => visual.phase === 'cancelled');
    await Promise.all(cancelled.map(([id]) => graph.rerun(id).catch(() => {})));
  }

  /**
   * Stops a run or step, and whatever it left running: an endless stream
   * stops too. Between ticks, it stops what earlier ticks left running.
   */
  stop(): Promise<void> {
    if (this.#attached) return this.#stopAttached(this.#attached);
    this.#stopRequested = true;
    if (!this.#ticking && this.#session) {
      // Sent now, in order with what else is asked: what it changes is polled once it is done.
      void this.#shutdown().then(() => this.#pollState());
    }
    for (const { resume } of this.#held.splice(0)) resume();
    this.#setStepping(false);
    // No line stops it on its way out, or the worker would answer nothing, and be taken to
    // be stuck: they are set again once it has stopped.
    this.#lineStop = null;
    this.#session?.lines?.release();
    if (this.#stopResolve) {
      this.#stopResolve();
      this.#stopResolve = null;
    }
    // Settled once the worker has said what stopped, or has been ended.
    const session = this.#session;
    if (session) this.#stopping ??= this.#stoppedOrEnded(session);
    return this.#stopping ?? Promise.resolve();
  }

  /**
   * Stops the program's graph, as one run here stops: what is under way is cancelled, and its
   * ticks advance nothing until it is run again, the cancelled nodes first. What it was held
   * at, a node or a line, is let go: as here, its breakpoints are set again when it runs.
   */
  async #stopAttached(session: AttachedSession): Promise<void> {
    const stopped = session.stop();
    for (const { resume } of this.#held.splice(0)) resume();
    this.#setStepping(false);
    this.#lineStop = null;
    session.lines?.release();
    this.#mode = 'idle';
    this.#dispatch('state-changed');
    await stopped;
  }

  /**
   * Sees a stop through. A worker that has not answered in a while is not reading what it
   * is sent, as a node that spins never returns to it, so it is ended, and the graph
   * loaded in another, on the inputs it was given.
   */
  async #stoppedOrEnded(session: DebugSession): Promise<void> {
    try {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const answered = await Promise.race([
        session.graph.sync().then(
          () => true,
          () => true,
        ),
        new Promise<boolean>((resolve) => {
          timer = setTimeout(() => resolve(false), STOP_GRACE_MS);
        }),
      ]);
      clearTimeout(timer);
      if (!answered && this.#session === session && this.#worker?.terminate) {
        await this.#replaceWorker();
      } else if (this.#session === session) {
        this.#armLineBreakpoints();
      }
    } finally {
      this.#stopping = null;
    }
  }

  async #replaceWorker(): Promise<void> {
    const running = [...this.#nodes.values()]
      .filter((node) => node.phase === 'running')
      .map((node) => `\`${node.id}\``);
    this.#session = null;
    this.#endWorker();
    this.#traceEvents.push({
      event: 'warning',
      message: `${running.join(', ') || 'A node'} did not stop, so its worker was ended and the graph loaded again: what its nodes held is gone`,
      timestamp: performance.now(),
    });
    this.#dispatch('trace-event');
    try {
      if (this.#graphJson) await this.#createSession(this.#graphJson);
      this.#pollState();
    } catch (error) {
      this.#dirty = true;
      const message = error instanceof Error ? error.message : String(error);
      this.#pushError(`Cannot load the graph again: ${message}`);
    }
    this.#dispatch('state-changed');
  }

  // -- Breakpoints ------------------------------------------------------------

  toggleBreakpoint(nodeId: string): void {
    if (this.#breakpoints.has(nodeId)) {
      this.#breakpoints.delete(nodeId);
    } else {
      this.#breakpoints.add(nodeId);
    }
    const visual = this.#nodes.get(nodeId);
    if (visual) visual.breakpoint = this.#breakpoints.has(nodeId);
    this.#sendHolds();
    this.#dispatch('state-changed');
  }

  // -- Undo / Redo ------------------------------------------------------------

  undo(): void {
    if (this.#locked()) return;
    const cmd = this.#undoStack.pop();
    if (!cmd) return;
    cmd.undo();
    this.#redoStack.push(cmd);
    this.#dispatch('state-changed');
  }

  redo(): void {
    if (this.#locked()) return;
    const cmd = this.#redoStack.pop();
    if (!cmd) return;
    cmd.execute();
    this.#undoStack.push(cmd);
    this.#dispatch('state-changed');
  }

  commitMove(nodeId: string, fromX: number, fromY: number, toX: number, toY: number): void {
    if (fromX === toX && fromY === toY) return;
    this.#undoStack.push({
      description: `Move ${nodeId}`,
      execute: () => this.moveNode(nodeId, toX, toY),
      undo: () => this.moveNode(nodeId, fromX, fromY),
    });
    if (this.#undoStack.length > MAX_UNDO) this.#undoStack.shift();
    this.#redoStack.length = 0;
  }

  // -- Copy / Paste -----------------------------------------------------------

  copySelected(): void {
    const nodeId = this.#selectedNode;
    if (!nodeId || !this.#graphJson) return;
    const graphNode = this.#graphJson.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    const visual = this.#nodes.get(nodeId);
    const ports = new Map<string, { inputs: PortVisual[]; outputs: PortVisual[] }>();
    if (visual) {
      ports.set(nodeId, {
        inputs: structuredClone(visual.inputs),
        outputs: structuredClone(visual.outputs),
      });
    }
    this.#clipboard = {
      nodes: [structuredClone(graphNode)],
      edges: [],
      ports,
    };
  }

  cutSelected(): void {
    this.copySelected();
    const nodeId = this.#selectedNode;
    if (nodeId) this.removeNode(nodeId);
  }

  paste(x: number, y: number): void {
    if (this.#locked()) return;
    if (!this.#clipboard || !this.#graphJson) return;
    const suffix = `-${Date.now().toString(36)}`;
    for (const node of this.#clipboard.nodes) {
      const newId = `${node.id}${suffix}`;
      const compRef = node.component;
      const source = this.componentSources.find((s) => {
        const graphComp = this.#graphJson!.components[s.index] as
          | Record<string, unknown>
          | undefined;
        return (
          graphComp && EditorController.#deepEqual(graphComp, compRef as Record<string, unknown>)
        );
      });
      if (!source) continue;
      // Prefer ports from the catalog source; fall back to ports saved in clipboard
      const savedPorts = this.#clipboard.ports.get(node.id);
      const inputs = source.inputs.length > 0 ? source.inputs : (savedPorts?.inputs ?? []);
      const outputs = source.outputs.length > 0 ? source.outputs : (savedPorts?.outputs ?? []);
      const copy = structuredClone(node);
      const newNode: GraphJsonNode = {
        ...copy,
        id: newId,
        // Its injected inputs, mocks, size and preview go with it.
        attrs: { ...copy.attrs, 'editor.x': String(x), 'editor.y': String(y) },
      };
      this.#graphJson.nodes.push(newNode);
      this.#nodes.set(
        newId,
        nodeVisual({
          id: newId,
          label: newNode.label,
          docs: source.docs,
          attrs: newNode.attrs,
          inputs,
          outputs,
          phase: 'pending',
          breakpoint: false,
        }),
      );
      x += 30;
      y += 30;
    }
    void this.#reloadAfterEdit();
  }

  // -- Inspection / Injection -------------------------------------------------

  async readOutput(node: string, port: string): Promise<unknown> {
    if (!this.#session) throw new Error('no session loaded');
    const kind = this.#nodes.get(node)?.outputs.find((p) => p.name === port)?.kind;
    return kind === 'handle'
      ? this.#session.graph.readHandle(node, port)
      : this.#session.graph.readOutput(node, port);
  }

  async readOutputWave(node: string, port: string): Promise<string | undefined> {
    if (!this.#session) throw new Error('no session loaded');
    return this.#session.graph.readOutputWave(node, port);
  }

  /**
   * Per node and port, WAVE text fetched for what the port latches, and the text of the
   * version before, shown until it is.
   */
  #waves = new Map<string, { version: number; text?: string; before?: string }>();

  /**
   * An output's latest value as WAVE text, for drawing: what was fetched
   * for its latest version, with a `state-changed` when it is there. While that is
   * being fetched, the text of the version before, so that a value changing at every
   * tick goes on showing; `undefined` only before any. An error's message stands in
   * for the text.
   */
  outputWave(node: string, port: string): string | undefined {
    const session = this.#session;
    if (!session) return undefined;
    const key = `${node}\0${port}`;
    let version: number;
    try {
      version = session.graph.outputVersion(node, port);
    } catch (error) {
      return error instanceof Error ? error.message : String(error);
    }
    const known = this.#waves.get(key);
    if (known?.version === version) return known.text ?? known.before;
    const entry: { version: number; text?: string; before?: string } = {
      version,
      before: known?.text ?? known?.before,
    };
    this.#waves.set(key, entry);
    session.graph.readOutputWave(node, port).then(
      (text: string | undefined) => {
        entry.text = text ?? '-';
        this.#dispatch('state-changed');
      },
      (error: unknown) => {
        entry.text = error instanceof Error ? error.message : String(error);
        this.#dispatch('state-changed');
      },
    );
    return entry.before;
  }

  #injectedValues = new Map<string, Map<string, string>>();

  async injectValue(node: string, port: string, wave: string): Promise<void> {
    if (!this.#session) throw new Error('no session loaded');
    await this.#session.graph.injectWave(node, port, wave);
    if (!this.#injectedValues.has(node)) this.#injectedValues.set(node, new Map());
    this.#injectedValues.get(node)!.set(port, wave);
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === node);
    if (graphNode) {
      if (!graphNode.attrs) graphNode.attrs = {};
      graphNode.attrs[`inject.${port}`] = wave;
    }
    this.#handOverInjection();
    this.#dispatch('state-changed');
  }

  async clearInput(node: string, port: string): Promise<void> {
    if (!this.#session) throw new Error('no session loaded');
    await this.#session.graph.clearInput(node, port);
    this.#injectedValues.get(node)?.delete(port);
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === node);
    if (graphNode?.attrs) {
      delete graphNode.attrs[`inject.${port}`];
    }
    this.#handOverInjection();
    this.#dispatch('state-changed');
  }

  /**
   * Takes the values `graph` says are injected (its nodes' `inject.<port>` attributes) as
   * those injected: an attached graph's program injects them itself, so they show here.
   */
  #adoptInjections(graph: GraphJson): void {
    this.#injectedValues.clear();
    for (const node of graph.nodes) {
      for (const [key, value] of Object.entries(node.attrs ?? {})) {
        if (!key.startsWith('inject.')) continue;
        let ports = this.#injectedValues.get(node.id);
        if (!ports) {
          ports = new Map();
          this.#injectedValues.set(node.id, ports);
        }
        ports.set(key.slice('inject.'.length), value);
      }
    }
  }

  /**
   * Hands an attached graph's program the graph with a value injected, or cleared, there:
   * it is the graph's (an `inject.<port>` attribute), so the program keeps it with the graph.
   * It is injected already, so the program need not load the graph again.
   */
  #handOverInjection(): void {
    if (this.#attached && this.#editing) void this.#reloadAfterEdit();
  }

  getInjectedValue(node: string, port: string): string | undefined {
    return this.#injectedValues.get(node)?.get(port);
  }

  // -- Mock management --------------------------------------------------------

  isMocked(nodeId: string): boolean {
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    return graphNode?.attrs?.['mock.enabled'] === 'true';
  }

  toggleMock(nodeId: string): void {
    if (this.#locked()) return;
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    if (!graphNode.attrs) graphNode.attrs = {};
    if (graphNode.attrs['mock.enabled'] === 'true') {
      delete graphNode.attrs['mock.enabled'];
    } else {
      graphNode.attrs['mock.enabled'] = 'true';
    }
    void this.#reloadAfterEdit();
  }

  setMockOutput(nodeId: string, port: string, wave: string): void {
    if (this.#locked()) return;
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    if (!graphNode.attrs) graphNode.attrs = {};
    graphNode.attrs[`mock.${port}`] = wave;
    if (this.isMocked(nodeId)) void this.#reloadAfterEdit();
    else this.#dispatch('state-changed');
  }

  clearMockOutput(nodeId: string, port: string): void {
    if (this.#locked()) return;
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode?.attrs) {
      delete graphNode.attrs[`mock.${port}`];
    }
    if (this.isMocked(nodeId)) void this.#reloadAfterEdit();
    else this.#dispatch('state-changed');
  }

  getMockOutputs(nodeId: string): ReadonlyMap<string, string> {
    const result = new Map<string, string>();
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!graphNode?.attrs) return result;
    for (const [key, value] of Object.entries(graphNode.attrs)) {
      if (key.startsWith('mock.') && key !== 'mock.enabled') {
        result.set(key.slice('mock.'.length), value);
      }
    }
    return result;
  }

  // -- Capability info --------------------------------------------------------

  nodeCapabilities(nodeId: string): readonly CapabilityInfo[] {
    return this.#capabilityInfo.get(nodeId) ?? [];
  }

  // -- Simulation / Snapshots -------------------------------------------------

  /**
   * Saves the graph's state under `name`. A state that cannot be written
   * (a value too large to convert, say) is reported in the trace, naming
   * the value, and nothing is saved.
   */
  async saveSnapshot(name?: string): Promise<void> {
    if (!this.#session) throw new Error('no session loaded');
    let data: string;
    try {
      data = await this.#session.graph.snapshot();
    } catch (err) {
      this.#pushError(`Snapshot failed: ${err instanceof Error ? err.message : String(err)}`);
      return;
    }
    const snapName = name ?? `snap-${this.#savedSnapshots.length + 1}`;
    this.#savedSnapshots.push({ name: snapName, data, timestamp: Date.now() });
    this.#dispatch('state-changed');
  }

  async restoreNamedSnapshot(name: string): Promise<void> {
    const snap = this.#savedSnapshots.find((s) => s.name === name);
    if (!snap) throw new Error(`no snapshot named "${name}"`);
    if (!this.#session) throw new Error('no session loaded');
    await this.#session.graph.restore(snap.data);
    this.#pollState();
    await this.#pullObserved(true);
  }

  deleteSnapshot(name: string): void {
    this.#savedSnapshots = this.#savedSnapshots.filter((s) => s.name !== name);
    this.#dispatch('state-changed');
  }

  // -- Selection --------------------------------------------------------------

  select(nodeId: string | null): void {
    this.#selectedNode = nodeId;
    if (nodeId !== null) this.#selectedEdge = null;
    this.#pullSoon();
    this.#dispatch('state-changed');
  }

  selectEdge(edgeId: string | null): void {
    this.#selectedEdge = edgeId;
    if (edgeId !== null) this.#selectedNode = null;
    this.#dispatch('state-changed');
  }

  // -- Graph editing ----------------------------------------------------------

  moveNode(nodeId: string, x: number, y: number): void {
    const visual = this.#nodes.get(nodeId);
    if (!visual) return;
    visual.x = x;
    visual.y = y;
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode) {
      if (!graphNode.attrs) graphNode.attrs = {};
      graphNode.attrs['editor.x'] = String(x);
      graphNode.attrs['editor.y'] = String(y);
    }
  }

  addNode(id: string, componentRef: object, x: number, y: number): void {
    if (this.#locked()) return;
    if (!this.#graphJson) throw new Error('no graph loaded');
    const node: GraphJsonNode = {
      id,
      component: componentRef,
      attrs: { 'editor.x': String(x), 'editor.y': String(y) },
    };
    this.#graphJson.nodes.push(node);
    this.#nodes.set(
      id,
      nodeVisual({
        id,
        attrs: node.attrs,
        inputs: [],
        outputs: [],
        phase: 'pending',
        breakpoint: false,
      }),
    );
    void this.#reloadAfterEdit();
  }

  removeNode(id: string): void {
    if (this.#locked()) return;
    if (!this.#graphJson) throw new Error('no graph loaded');
    const savedNode = this.#graphJson.nodes.find((n) => n.id === id);
    const savedConns = this.#graphJson.connections.filter(
      (c) => c.from.node === id || c.to.node === id,
    );
    const savedVisual = this.#nodes.get(id);
    const savedEdges = this.#edges.filter((e) => e.fromNode === id || e.toNode === id);
    if (!savedNode) return;

    this.#undoStack.push({
      description: `Delete ${id}`,
      execute: () => this.#doRemoveNode(id),
      undo: () => {
        this.#graphJson!.nodes.push(structuredClone(savedNode));
        for (const c of savedConns) this.#graphJson!.connections.push(structuredClone(c));
        if (savedVisual) this.#nodes.set(id, structuredClone(savedVisual));
        for (const e of savedEdges) this.#edges.push(structuredClone(e));
        void this.#reloadAfterEdit();
      },
    });
    if (this.#undoStack.length > MAX_UNDO) this.#undoStack.shift();
    this.#redoStack.length = 0;

    this.#doRemoveNode(id);
  }

  #doRemoveNode(id: string): void {
    if (!this.#graphJson) return;
    this.#graphJson.nodes = this.#graphJson.nodes.filter((n) => n.id !== id);
    this.#graphJson.connections = this.#graphJson.connections.filter(
      (c) => c.from.node !== id && c.to.node !== id,
    );
    this.#nodes.delete(id);
    this.#forgetObserved(id);
    this.#edges = this.#edges.filter((e) => e.fromNode !== id && e.toNode !== id);
    if (this.#selectedNode === id) this.#selectedNode = null;
    void this.#reloadAfterEdit();
  }

  addConnection(id: string, from: GraphJsonPortRef, to: GraphJsonPortRef, feedback = false): void {
    if (this.#locked()) return;
    if (!this.#graphJson) throw new Error('no graph loaded');
    const asFeedback = feedback
      ? this.canBeFeedback(from.node, from.port, to.node, to.port)
      : { ok: true };
    if (!asFeedback.ok) {
      feedback = false;
      this.#pushError(`Feedback downgraded: ${asFeedback.reason}`);
    }
    const check = this.canConnect(from.node, from.port, to.node, to.port, feedback);
    if (!check.ok) {
      this.#pushError(`Cannot connect: ${check.reason}`);
      return;
    }
    const conn: GraphJsonConnection = {
      id,
      from: { node: from.node, port: from.port },
      to: { node: to.node, port: to.port },
      ...(feedback ? { feedback: true } : {}),
    };
    this.#graphJson.connections.push(conn);
    this.#edges.push({
      id,
      fromNode: from.node,
      fromPort: from.port,
      toNode: to.node,
      toPort: to.port,
      feedback,
    });
    this.#selectedEdge = id;
    this.#selectedNode = null;
    void this.#reloadAfterEdit();
  }

  removeConnection(id: string): void {
    if (this.#locked()) return;
    if (!this.#graphJson) throw new Error('no graph loaded');
    const savedConn = this.#graphJson.connections.find((c) => c.id === id);
    const savedEdge = this.#edges.find((e) => e.id === id);
    if (!savedConn) return;

    this.#undoStack.push({
      description: `Delete connection ${id}`,
      execute: () => this.#doRemoveConnection(id),
      undo: () => {
        this.#graphJson!.connections.push(structuredClone(savedConn));
        if (savedEdge) this.#edges.push({ ...savedEdge });
        if (this.#selectedEdge === null) this.#selectedEdge = id;
        void this.#reloadAfterEdit();
      },
    });
    if (this.#undoStack.length > MAX_UNDO) this.#undoStack.shift();
    this.#redoStack.length = 0;

    this.#doRemoveConnection(id);
  }

  #doRemoveConnection(id: string): void {
    if (!this.#graphJson) return;
    this.#graphJson.connections = this.#graphJson.connections.filter((c) => c.id !== id);
    this.#edges = this.#edges.filter((e) => e.id !== id);
    if (this.#selectedEdge === id) this.#selectedEdge = null;
    void this.#reloadAfterEdit();
  }

  // -- Serialization ----------------------------------------------------------

  exportGraph(): string {
    return JSON.stringify(this.#graphJson, null, 2);
  }

  downloadGraph(filename = 'graph.json'): void {
    const json = this.exportGraph();
    const blob = new Blob([json], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  }

  async downloadBundle(filename = 'graph-bundle.json'): Promise<void> {
    if (!this.#originalOptions) throw new Error('no graph loaded');
    const graph = JSON.parse(this.exportGraph());
    const components = this.#originalOptions.components.map((c) => ({
      id: c.id,
      wit: c.wit,
      bytes: btoa(Array.from(new Uint8Array(c.bytes), (b) => String.fromCharCode(b)).join('')),
    }));
    const bundle = JSON.stringify({ version: 1, graph, components }, null, 2);
    const blob = new Blob([bundle], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  }

  async snapshot(): Promise<string> {
    if (!this.#session) throw new Error('no session loaded');
    return this.#session.graph.snapshot();
  }

  async restore(snapshot: string): Promise<void> {
    if (!this.#session) throw new Error('no session loaded');
    await this.#session.graph.restore(snapshot);
    this.#pollState();
    await this.#pullObserved(true);
  }

  // -- Auto-layout ------------------------------------------------------------

  async autoLayout(): Promise<void> {
    const layoutNodes: LayoutNode[] = [...this.#nodes.values()].map((n) => ({
      id: n.id,
      width: n.width,
      height: n.height,
      inputs: n.inputs.map((p) => ({ name: p.name, y: portOffset(n, 'input', p.name)?.y })),
      outputs: n.outputs.map((p) => ({ name: p.name, y: portOffset(n, 'output', p.name)?.y })),
    }));
    const layoutEdges: LayoutEdge[] = this.#edges.map((e) => ({
      id: e.id,
      fromNode: e.fromNode,
      fromPort: e.fromPort,
      toNode: e.toNode,
      toPort: e.toPort,
    }));
    const positions = await autoLayout(layoutNodes, layoutEdges);
    for (const [id, pos] of positions) this.moveNode(id, pos.x, pos.y);
    this.#dispatch('state-changed');
  }

  // -- Internal ---------------------------------------------------------------

  async #createSession(graph: GraphJson): Promise<void> {
    if (!this.#originalOptions) throw new Error('no options');

    // Build mocks: merge caller-supplied mocks with attr-driven mocks
    const callerMocks = this.#originalOptions.mocks ?? {};
    const attrMocks: Record<string, (inputs: PortValues) => PortValues> = {};
    for (const gNode of graph.nodes) {
      if (gNode.attrs?.['mock.enabled'] !== 'true') continue;
      if (Object.hasOwn(callerMocks, gNode.id)) continue; // caller takes precedence
      const outputs: Record<string, string> = {};
      for (const [key, value] of Object.entries(gNode.attrs)) {
        if (key.startsWith('mock.') && key !== 'mock.enabled') {
          outputs[key.slice('mock.'.length)] = value;
        }
      }
      attrMocks[gNode.id] = () => {
        const result: PortValues = {};
        for (const [port, wave] of Object.entries(outputs)) {
          // Pass WAVE text through as-is; the scheduler resolves it
          result[port] = wave;
        }
        return result;
      };
    }
    const mocks = { ...attrMocks, ...callerMocks };

    // Set once loaded: until then, and once replaced, its hooks are stale.
    let session: RemoteSession | null = null;
    this.#worker ??= this.#originalOptions.worker();
    const isolated =
      (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated !== false;
    if (this.#originalOptions.lines && !isolated) this.#warnOfNoLines();
    session = await RemoteSession.load(this.#worker, {
      ...(this.#originalOptions.lines && isolated
        ? {
            lines: {
              ...(typeof this.#originalOptions.lines === 'object'
                ? this.#originalOptions.lines
                : {}),
              onStop: (stop: LineStop): void => {
                if (this.#session !== session) return;
                // On its way out: nothing is to look at it.
                if (this.#stopRequested) {
                  session?.lines?.resume();
                  return;
                }
                this.#stopOnLine(stop);
              },
            },
          }
        : {}),
      graph: graph,
      components: this.#originalOptions.components,
      wasm: this.#originalOptions.wasm,
      mocks: Object.keys(mocks).length > 0 ? mocks : undefined,
      holds: this.#holds(),

      beforeRun: (node, held): Promise<void> => this.#beforeRun(session, node, held),

      afterRun: (node): void => this.#stepReturned(node),

      onStreamItem: ({ node, port, items }): void => {
        if (this.#session === session) this.#recordItems(node, port, items);
      },

      onFutureValue: ({ node, port, value }): void => {
        if (this.#session === session) this.#record(node, port, value);
      },

      onHandle: ({ node, port, shown, count }): void => {
        if (this.#session === session) this.#recordHandle(node, port, shown, count);
      },

      onState: (): void => {
        if (this.#session !== session) return;
        this.#pollState();
        void this.#pullObserved(false);
        this.#dispatch('state-changed');
      },
    });
    this.#session = session;
    this.#compileDiagnostics = [...session.diagnostics];
    // Each once, though every edit loads the graph again.
    for (const message of session.warnings) {
      if (this.#warned.has(message)) continue;
      this.#warned.add(message);
      this.#traceEvents.push({ event: 'warning', message, timestamp: performance.now() });
      this.#dispatch('trace-event');
    }
    this.#streamGroups = session.graph.streamGroups() as string[][];
    this.#dirty = false;
    this.#buildVisuals();
    this.#observedSent = '';
    this.#sendObserved();
    this.#armLineBreakpoints();
    await this.#restoreInjections(graph);
    this.#collectCapabilities();
  }

  /** A node's run is about to start: held, it waits here until let go. */
  async #beforeRun(session: DebugSession | null, node: string, held: boolean): Promise<void> {
    const breakpoint = this.#breakpoints.has(node);
    // Held, it waits for this. One no longer wanted held just goes on.
    if (held && (breakpoint || this.#stepping)) {
      // Read when it is hit, as nothing crosses to the host unasked.
      const inputs = await this.#heldInputs(session, node);
      if (this.#stopRequested) return;
      return new Promise<void>((resume) => {
        this.#held.push({ node, inputs, resume, breakpoint });
        this.#showStop();
      });
    }
    // Shown running until the tick ends, however long its run takes
    // (one reading a stream, say).
    const visual = this.#nodes.get(node);
    if (visual && visual.phase !== 'running') {
      visual.phase = 'running';
      this.#dispatch('state-changed');
    }
    // Its futures are new: until they resolve, the last ones are stale.
    for (const port of visual?.outputs ?? []) {
      if (port.kind === 'future') this.#observation(node, port.name).pending = true;
    }
  }

  /** A held run's Value inputs, as WAVE text by port name: as its outputs are shown. */
  async #heldInputs(
    session: DebugSession | null,
    node: string,
  ): Promise<BreakpointState['inputs']> {
    const inputs: BreakpointState['inputs'] = {};
    for (const port of this.#nodes.get(node)?.inputs ?? []) {
      if (port.kind !== 'value') continue;
      inputs[port.name] = await session?.graph
        .readInputWave(node, port.name)
        .catch(() => undefined);
    }
    return inputs;
  }

  #collectCapabilities(): void {
    this.#capabilityInfo.clear();
    if (!this.#session) return;
    for (const [nodeId, contract] of this.#session.contracts) {
      if (!contract.capabilities || contract.capabilities.length === 0) continue;
      // Loaded, so each was provided: instantiating throws for one that is not.
      const infos: CapabilityInfo[] = contract.capabilities.map((cap) => ({
        interface: cap.name,
        implements: cap.implements,
        satisfied: true,
      }));
      this.#capabilityInfo.set(nodeId, infos);
    }
  }

  async #restoreInjections(graph: GraphJson): Promise<void> {
    if (!this.#session) return;
    // A new session holds only what is injected into it here.
    this.#injectedValues.clear();
    for (const node of graph.nodes) {
      if (!node.attrs) continue;
      for (const [key, value] of Object.entries(node.attrs)) {
        if (!key.startsWith('inject.')) continue;
        const port = key.slice('inject.'.length);
        try {
          await this.#session.graph.injectWave(node.id, port, value);
          if (!this.#injectedValues.has(node.id)) this.#injectedValues.set(node.id, new Map());
          this.#injectedValues.get(node.id)!.set(port, value);
        } catch {
          // Port may not exist or may be connected — skip silently
        }
      }
    }
  }

  #buildVisuals(): void {
    if (!this.#session || !this.#graphJson) return;
    this.#nodes.clear();
    this.#edges = [];
    const contracts = this.#session.contracts;

    for (const graphNode of this.#graphJson.nodes) {
      const contract = contracts.get(graphNode.id);
      const visual: NodeVisual = nodeVisual({
        id: graphNode.id,
        label: graphNode.label,
        docs: contract?.docs,
        attrs: graphNode.attrs,
        inputs: (contract?.inputs ?? []).map(portVisual),
        outputs: (contract?.outputs ?? []).map(portVisual),
        phase: 'idle',
        breakpoint: this.#breakpoints.has(graphNode.id),
      });

      try {
        const state = this.#session.graph.nodeState(graphNode.id) as NodeState;
        visual.phase = state.phase;
        if (state.fault) visual.fault = state.fault;
        if (state.culprit) visual.culprit = state.culprit;
      } catch {}

      this.#nodes.set(graphNode.id, visual);
    }

    for (const conn of this.#graphJson.connections) {
      this.#edges.push({
        id: conn.id,
        fromNode: conn.from.node,
        fromPort: conn.from.port,
        toNode: conn.to.node,
        toPort: conn.to.port,
        feedback: conn.feedback ?? false,
      });
    }
  }

  #pollState(): void {
    if (!this.#session) return;

    const now = performance.now();
    const trace = (this.#session.graph.takeTrace() as TraceEntry[]).map((e) => ({
      ...e,
      timestamp: e.timestamp ?? now,
    }));
    if (trace.length > 0) {
      this.#traceEvents.push(...trace);
      if (this.#traceEvents.length > MAX_TRACE_ENTRIES) {
        this.#traceEvents = this.#traceEvents.slice(-MAX_TRACE_ENTRIES);
      }
      this.#dispatch('trace-event');
    }

    // A tick reports its own, and has the scheduler until it ends.
    const faults = this.#ticking ? [] : (this.#session.graph.takeFaults() as FaultReport[]);
    for (const fault of faults) {
      // A fault stops the whole graph.
      for (const node of this.#nodes.values()) {
        node.fault = fault.fault;
        if (fault.culprit) node.culprit = fault.culprit;
      }
      this.#dispatch('fault');
    }

    for (const [id, visual] of this.#nodes) {
      try {
        const state = this.#session.graph.nodeState(id) as NodeState;
        // Held before its run, it has not started, whatever the scheduler has sent it.
        visual.phase = this.#held.some((held) => held.node === id) ? 'pending' : state.phase;
        visual.fault = state.fault;
        visual.culprit = state.culprit;
      } catch {}
      // A faulted or cancelled run's futures never resolve: they are not
      // pending, but gone.
      if (visual.phase === 'faulted' || visual.phase === 'cancelled') {
        for (const port of visual.outputs) {
          const seen = this.#observed.get(id)?.get(port.name);
          if (port.kind === 'future' && seen) seen.pending = false;
        }
      }
    }

    this.#dispatch('state-changed');
  }

  /**
   * Records what the observed Value and handle outputs carry, if they carry
   * something new: the latched value itself, never converted (to WAVE text,
   * say). `final` ignores the preview interval, as a run ends.
   */
  async #pullObserved(final: boolean): Promise<void> {
    const pull = this.#pull(final);
    this.#pulls.add(pull);
    void pull.finally(() => this.#pulls.delete(pull));
    // One under way claimed what it reads: done means that is in too.
    await Promise.all(this.#pulls);
  }

  /** The reads of outputs under way. */
  #pulls = new Set<Promise<void>>();

  async #pull(final: boolean): Promise<void> {
    const session = this.#session;
    if (!session) return;
    for (const [id, visual] of this.#nodes) {
      for (const port of visual.outputs) {
        if ((port.kind !== 'value' && port.kind !== 'handle') || !this.isObserved(id, port.name))
          continue;
        const seen = this.#observed.get(id)?.get(port.name);
        if (!final && this.#previewsLag(seen)) continue;
        try {
          const version = session.graph.outputVersion(id, port.name);
          if (version === (seen?.version ?? 0)) continue;
          // Claimed before the wait, so an overlapping pull reads it no more.
          const claimed = this.#observation(id, port.name);
          claimed.version = version;
          claimed.pulledAt = performance.now();
          const value =
            port.kind === 'handle'
              ? await session.graph.readHandle(id, port.name)
              : await session.graph.readOutput(id, port.name);
          if (this.#session !== session || value === undefined) continue;
          if (port.kind === 'handle') this.#recordHandle(id, port.name, value, 1);
          else this.#record(id, port.name, value);
        } catch {}
      }
    }
  }

  /** Pulls what became observed, or new, outside of a tick slice. */
  #pullSoon(): void {
    void this.#pullObserved(true).then(() => this.#dispatch('state-changed'));
  }

  #dispatchQueued = false;
  #outputQueued = false;
  #dispatch(type: string): void {
    if (type === 'state-changed') {
      if (this.#dispatchQueued) return;
      this.#dispatchQueued = true;
      queueMicrotask(() => {
        this.#dispatchQueued = false;
        // Whatever changed may have changed what is looked at.
        this.#sendObserved();
        this.dispatchEvent(new CustomEvent(type));
      });
    } else if (type === 'output-changed') {
      // Once a frame, however many stream items arrive in it.
      if (this.#outputQueued) return;
      this.#outputQueued = true;
      const fire = () => {
        this.#outputQueued = false;
        this.dispatchEvent(new CustomEvent(type));
      };
      if (typeof requestAnimationFrame === 'function') requestAnimationFrame(fire);
      else setTimeout(fire, 16);
    } else {
      this.dispatchEvent(new CustomEvent(type));
    }
  }

  /** Whether an edit is refused for a run: it is, and the trace says so. */
  #locked(): boolean {
    if (this.editable) return false;
    const message = this.#attached
      ? this.#editing
        ? 'The graph cannot be edited until it is stopped: stop it first'
        : "An attached graph is the program's: it cannot be edited from here"
      : 'The graph cannot be edited while it runs: stop it first';
    const last = this.#traceEvents.at(-1);
    // A key held down, say, says it once.
    if (last?.event === 'warning' && last.message === message) return true;
    this.#traceEvents.push({ event: 'warning', message, timestamp: performance.now() });
    if (this.#traceEvents.length > MAX_TRACE_ENTRIES) {
      this.#traceEvents = this.#traceEvents.slice(-MAX_TRACE_ENTRIES);
    }
    this.#dispatch('trace-event');
    return true;
  }

  #pushError(message: string): void {
    this.#traceEvents.push({ event: 'error', message, timestamp: performance.now() });
    if (this.#traceEvents.length > MAX_TRACE_ENTRIES) {
      this.#traceEvents = this.#traceEvents.slice(-MAX_TRACE_ENTRIES);
    }
    this.#dispatch('trace-event');
  }

  async #reload(): Promise<void> {
    await this.#stopping;
    if (this.#attached && this.#editing && this.#graphJson) {
      await this.#applyAttached(this.#editing, this.#graphJson);
      return;
    }
    if (!this.#originalOptions || !this.#graphJson) return;
    const oldSession = this.#session;
    this.#session = null;
    try {
      await this.#createSession(this.#graphJson);
      oldSession?.dispose();
    } catch (err) {
      // Restore old session if reload fails — keeps visuals intact
      this.#session = oldSession;
      this.#dirty = true;
      throw err;
    }
  }

  /**
   * Hands `draft` to the program attached to, which runs it, and attaches to that. Edits made
   * meanwhile stay, to be handed over next.
   */
  async #applyAttached(editing: AttachedEditing, draft: GraphJson): Promise<void> {
    const sent = JSON.stringify(draft);
    const next = await editing.apply(JSON.parse(sent) as GraphJson);
    if (next) {
      const selected = this.#selectedNode;
      await this.attach({ ...next, editing });
      // Drawn where the editor had them, edits made meanwhile with them.
      this.#graphJson = draft;
      this.#buildVisuals();
      if (selected && this.#nodes.has(selected)) this.#selectedNode = selected;
    }
    this.#dirty = JSON.stringify(draft) !== sent;
  }

  #reloading = false;
  #pendingReload = false;
  async #reloadAfterEdit(): Promise<void> {
    this.#dirty = true;
    if (this.#reloading) {
      this.#pendingReload = true;
      return;
    }
    this.#reloading = true;
    try {
      const hadError = this.#compileError !== null;
      try {
        await this.#reload();
        this.#compileError = null;
        if (hadError) {
          this.#traceEvents.push({
            event: 'compile-ok',
            timestamp: performance.now(),
            message: 'Graph compiles successfully',
          });
          this.#dispatch('trace-event');
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        this.#compileError = msg;
        this.#compileDiagnostics = diagnosticsOf(err) ?? [];
        this.#pushError(`Graph reload failed: ${msg}`);
      }
    } finally {
      this.#reloading = false;
    }
    this.#dispatch('state-changed');
    if (this.#pendingReload) {
      this.#pendingReload = false;
      void this.#reloadAfterEdit();
    }
  }
}
