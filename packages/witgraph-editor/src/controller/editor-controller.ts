import {
  type Contract,
  type Diagnostic,
  describeComponent,
  diagnosticsOf,
  GraphChecker,
  type PortValues,
  Session,
  type SessionOptions,
} from 'witgraph-loader';
import { NODE_HEADER_HEIGHT } from '../components/styles.ts';
import { weight } from '../util/weight.ts';
import { viewerFor } from '../viewers/registry.ts';
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
  NodeState,
  NodeVisual,
  OutputView,
  PortVisual,
  SavedSnapshot,
  TraceEntry,
} from './types.ts';
import { describeHandle, nodeSize, nodeVisual, portVisual } from './visual.ts';

const MAX_TRACE_ENTRIES = 1000;
/**
 * The longest a tick runs before it is interrupted: work that never ends (an
 * endless stream) carries on into the next tick, and the rest of the graph
 * (a feedback loop, say) still advances a tick at a time beside it.
 */
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
  /** Whether `latest` is ours to dispose: a reference a viewer kept. */
  kept: boolean;
  bytes: number;
}

/** Disposes what a viewer kept of an output's latest item. */
function release(seen: Observation): void {
  if (!seen.kept) return;
  seen.kept = false;
  const kept = seen.latest as Partial<Disposable> | null | undefined;
  try {
    kept?.[Symbol.dispose]?.();
  } catch (error) {
    console.error('disposing what a viewer kept threw:', error);
  }
}

/** The most edits undo goes back over. */
const MAX_UNDO = 50;

/** An output that can feed an input, as `compatibleOutputsForInput` lists them. */
export interface CompatibleOutput {
  node: string;
  port: string;
  label: string;
  self: boolean;
}

export class EditorController extends EventTarget {
  #session: Session | null = null;
  #originalOptions: EditorLoadOptions | null = null;
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
  #breakpointResolve: (() => void) | null = null;
  #pausedNode: string | null = null;
  #breakpointState: BreakpointState | null = null;

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
  /** The errors that stop the graph compiling. */
  #compileDiagnostics: Diagnostic[] = [];
  /** Compiles what the graph would be, by the rules a load goes by. */
  #checker: GraphChecker | null = null;
  /**
   * What can feed each input, for as long as the graph stays as `graph` says. Finding
   * out compiles once per output, and the inspector asks on every state change.
   */
  #compatible: { graph: string; outputs: Map<string, readonly CompatibleOutput[]> } | null = null;
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
    return this.#pausedNode;
  }
  get breakpointState(): BreakpointState | null {
    return this.#breakpointState;
  }
  get lastRunDurationMs(): number | null {
    return this.#lastRunDurationMs;
  }
  get compileError(): string | null {
    return this.#compileError;
  }
  /** What stops the graph compiling. Warnings are not here: they stop nothing. */
  get compileDiagnostics(): readonly Diagnostic[] {
    return this.#compileDiagnostics;
  }
  /** What stops the graph compiling that is about a node, or a connection of it. */
  diagnosticsForNode(id: string): readonly Diagnostic[] {
    return this.#compileDiagnostics.filter(({ location }) => {
      if (location.nodes.includes(id)) return true;
      const edge = this.#edges.find((e) => e.id === location.connection);
      return edge?.fromNode === id || edge?.toNode === id;
    });
  }
  /** What stops the graph compiling that is about a connection. */
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
    return this.#mode !== 'running' && this.#mode !== 'paused';
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
   * previews it, or the node is selected (the inspector shows its outputs).
   * Nothing else is recorded or read back, so an output nobody looks at
   * costs nothing however much it carries.
   */
  isObserved(node: string, port: string): boolean {
    return this.#selectedNode === node || this.#nodes.get(node)?.preview?.port === port;
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

  /** The resource an output carries, if it carries one. */
  #resourceOf(node: string, port: string): string | undefined {
    return this.#nodes.get(node)?.outputs.find((p) => p.name === port)?.handle?.resource;
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
        kept: false,
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
   * Records a handle a stream or future carried. It is its reader's, and may
   * be gone by the time anything is drawn, so what stays is what the preview's
   * viewer keeps of it, or else a description. No history: each would hold on
   * to what the handle holds.
   */
  #recordHandle(node: string, port: string, item: unknown, resource: string): void {
    const visual = this.#nodes.get(node);
    const viewer = visual?.preview?.port === port ? viewerFor(visual.preview.viewer) : undefined;
    const kept = viewer?.keep?.(item);
    const seen = this.#observation(node, port);
    release(seen);
    seen.latest = kept ?? describeHandle(item, resource);
    seen.kept = kept !== undefined;
    seen.recent = [seen.latest];
    seen.pending = false;
    seen.count += 1;
    this.#dispatch('output-changed');
  }

  /** Forgets what outputs carried, disposing what was kept of it. */
  #forgetObserved(node?: string): void {
    const nodes = node === undefined ? [...this.#observed.values()] : [this.#observed.get(node)];
    for (const ports of nodes) for (const seen of ports?.values() ?? []) release(seen);
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
    if (!this.#session || !this.#originalOptions) return [];
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
      const source = this.#originalOptions!.components[compIdx];
      if (!source) continue;
      result.push({
        id: source.id,
        index: compIdx,
        docs: this.#session.contracts.get(nodeId)?.docs,
        inputs: visual.inputs,
        outputs: visual.outputs,
      });
    }

    // Also include components not yet used by any node — derive ports
    // from the component's WIT contract so the catalog and paste show them.
    for (let i = 0; i < this.#originalOptions.components.length; i++) {
      if (seen.has(i)) continue;
      const comp = this.#originalOptions.components[i];
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
    if (!this.#graphJson || !this.#originalOptions) throw new Error('no graph loaded');
    const comp = this.#originalOptions.components[componentIndex];
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
    // Update breakpoints
    if (this.#breakpoints.has(oldId)) {
      this.#breakpoints.delete(oldId);
      this.#breakpoints.add(newId);
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

  canBeFeedback(fromNode: string, fromPort: string): boolean {
    const node = this.#nodes.get(fromNode);
    const port = node?.outputs.find((p) => p.name === fromPort);
    return port?.kind === 'value' || port?.kind === 'handle';
  }

  /** What compiles graphs against the loaded components, once one has loaded. */
  #graphChecker(): GraphChecker | null {
    if (this.#checker) return this.#checker;
    if (!this.#originalOptions) return null;
    try {
      this.#checker = new GraphChecker(this.#originalOptions.components);
    } catch {
      // The module has yet to load, or a load will say what is wrong with them.
      return null;
    }
    return this.#checker;
  }

  /**
   * Why the graph could not have this connection, by the rules a load goes by: only
   * what is wrong with the connection itself. A cycle is not, as marking a connection
   * of it feedback is the next edit, and the graph says so until then.
   */
  #connectionProblem(
    from: GraphJsonPortRef,
    to: GraphJsonPortRef,
    feedback: boolean,
    replacing: boolean,
  ): string | undefined {
    const graph = this.#graphJson;
    const checker = this.#graphChecker();
    if (!graph || !checker) return undefined;
    let id = 'new';
    for (let at = 2; graph.connections.some((c) => c.id === id); at++) id = `new-${at}`;
    const samePort = (a: GraphJsonPortRef, node?: string, port?: string) =>
      a.node === node && a.port === port;
    const others = graph.connections.filter(
      (c) => !(replacing && samePort(c.to, to.node, to.port)),
    );
    const candidate = {
      ...graph,
      connections: [...others, { id, from, to, ...(feedback ? { feedback: true } : {}) }],
    };
    let said: Diagnostic[];
    try {
      said = checker.check(JSON.stringify(candidate)) as Diagnostic[];
    } catch {
      // Nothing to say of this connection: the next load says what is wrong.
      return undefined;
    }
    return said.find(({ severity, location }) => {
      if (severity !== 'error') return false;
      if (location.connection === id) return true;
      if (location.kind !== 'port') return false;
      const [node] = location.nodes;
      return samePort(from, node, location.port) || samePort(to, node, location.port);
    })?.message;
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
  ): { ok: boolean; reason?: string } {
    const reason = this.#connectionProblem(
      { node: fromNode, port: fromPort },
      { node: toNode, port: toPort },
      feedback,
      false,
    );
    return reason === undefined ? { ok: true } : { ok: false, reason };
  }

  compatibleOutputsForInput(nodeId: string, portName: string): readonly CompatibleOutput[] {
    const graph = `${JSON.stringify(this.#graphJson)}\u0000${[...this.#nodes.keys()].join()}`;
    if (this.#compatible?.graph !== graph) this.#compatible = { graph, outputs: new Map() };
    const key = `${nodeId}\u0000${portName}`;
    let outputs = this.#compatible.outputs.get(key);
    if (!outputs) {
      outputs = this.#findCompatibleOutputs(nodeId, portName);
      this.#compatible.outputs.set(key, outputs);
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
      feedback && this.canBeFeedback(fromNode, fromPort),
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
    if (feedback && !this.canBeFeedback(edge.fromNode, edge.fromPort)) {
      this.#pushError('Feedback is only valid on Value ports');
      return;
    }
    edge.feedback = feedback;
    const conn = this.#graphJson?.connections.find((c) => c.id === connId);
    if (conn) conn.feedback = feedback;
    void this.#reloadAfterEdit();
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
      wasm: prev?.wasm,
      capabilities: prev?.capabilities,
      mocks: prev?.mocks,
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

  async load(options: EditorLoadOptions): Promise<void> {
    this.#originalOptions = options;
    this.#forgetChecker();
    const graphJson: GraphJson =
      typeof options.graph === 'string'
        ? (JSON.parse(options.graph) as GraphJson)
        : structuredClone(options.graph);
    this.#graphJson = graphJson;
    await this.#createSession(graphJson);
    this.#compileError = null;
    this.#compileDiagnostics = [];
    this.#mode = 'idle';
    this.#dispatch('state-changed');
  }

  /** Lets the checker go, and what it said: it knows the components of one load. */
  #forgetChecker(): void {
    this.#checker?.free();
    this.#checker = null;
    this.#compatible = null;
  }

  dispose(): void {
    this.#forgetChecker();
    this.#session?.dispose();
    this.#session = null;
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
    this.#breakpointResolve = null;
    this.#pausedNode = null;
    this.#breakpointState = null;
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

  async run(): Promise<void> {
    try {
      if (this.#dirty || !this.#session) await this.#reload();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`Cannot run: ${msg}`);
      this.#mode = 'error';
      this.#dispatch('state-changed');
      return;
    }
    if (!this.#session) {
      this.#pushError('Cannot run: graph does not compile');
      this.#mode = 'error';
      this.#dispatch('state-changed');
      return;
    }

    this.#mode = 'running';
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#stopRequested = false;
    this.#rerunCancelled();
    this.#dispatch('state-changed');

    const startTime = performance.now();
    let ticks = 0;
    try {
      // Until the graph settles or is stopped: a graph with an endless
      // stream or a feedback loop runs until it is stopped.
      for (;;) {
        const started = performance.now();
        const result = await this.#tick();
        ticks++;
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
      this.#pushError(`Run failed: ${msg}`);
    } finally {
      const mode = this.#mode as EditorMode;
      if (mode === 'running' || mode === 'paused') this.#mode = 'idle';
      await this.#pullObserved(true);
      this.#lastRunDurationMs = performance.now() - startTime;
      this.#lastRunTicks = ticks;
      this.#dispatch('state-changed');
    }
  }

  async step(): Promise<void> {
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
    this.#rerunCancelled();
    this.#dispatch('state-changed');

    const startTime = performance.now();
    try {
      // One tick, at most a slice long: what is still running carries on
      // until the next tick, or until Stop.
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
    this.#dispatch('state-changed');
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
    this.#ticking = true;
    let tick: Awaited<ReturnType<Session['tick']>>;
    try {
      // Paused at a breakpoint, a tick ends only when resumed or stopped.
      const stop = Promise.race([stopped, slice.then(() => this.#paused())]);
      tick = await session.tick(stop);
    } finally {
      this.#ticking = false;
      this.#stopResolve = null;
      clearTimeout(timer);
    }
    this.#tickCount++;
    const stop = this.#stopRequested;
    if (stop) this.#shutdown();
    this.#pollState();
    await this.#pullObserved(stop);
    if (stop) return 'stopped';
    // Only `fatal` ends a run: past any other fault, ticking rebuilds what is owed.
    if (tick.result === 'faulted') return tick.fault.kind === 'fatal' ? 'aborted' : 'progress';
    return tick.result === 'idle' ? 'idle' : 'progress';
  }

  /** Resolves once no node is held at a breakpoint. */
  async #paused(): Promise<void> {
    while (this.#breakpointResolve && !this.#stopRequested) await nextFrame();
  }

  /** Stops the graph: what is still running ends, streams included. */
  #shutdown(): void {
    try {
      this.#session?.graph.shutdown();
    } catch {}
  }

  /** Nodes a stop cancelled run again on their latched inputs. */
  #rerunCancelled(): void {
    for (const [id, visual] of this.#nodes) {
      if (visual.phase !== 'cancelled') continue;
      try {
        this.#session?.graph.rerun(id);
      } catch {}
    }
  }

  /**
   * Stops a run or step, and whatever it left running: an endless stream
   * stops too. Between ticks, it stops what earlier ticks left running.
   */
  stop(): void {
    this.#stopRequested = true;
    if (!this.#ticking && this.#session) {
      this.#shutdown();
      this.#pollState();
    }
    if (this.#breakpointResolve) {
      this.#breakpointResolve();
      this.#breakpointResolve = null;
      this.#pausedNode = null;
      this.#breakpointState = null;
    }
    if (this.#stopResolve) {
      this.#stopResolve();
      this.#stopResolve = null;
    }
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

  resumeBreakpoint(): void {
    if (this.#breakpointResolve) {
      this.#breakpointResolve();
      this.#breakpointResolve = null;
      this.#pausedNode = null;
      this.#breakpointState = null;
      this.#mode = 'running';
      this.#dispatch('state-changed');
    }
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

  /** Per node and port, WAVE text fetched for what the port latches. */
  #waves = new Map<string, { version: number; text?: string }>();

  /**
   * An output's latest value as WAVE text, for drawing: what was fetched
   * for its latest version, or `undefined` while that is being fetched, with
   * a `state-changed` when it is there. An error's message stands in for the
   * text.
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
    if (known?.version === version) return known.text;
    const entry: { version: number; text?: string } = { version };
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
    return known?.text;
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
    this.#dispatch('state-changed');
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
    if (feedback && !this.canBeFeedback(from.node, from.port)) {
      feedback = false;
      this.#pushError('Feedback downgraded: only Value ports support feedback');
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
      portsBottom: NODE_HEADER_HEIGHT + n.portsHeight,
      inputs: n.inputs.map((p) => ({ name: p.name })),
      outputs: n.outputs.map((p) => ({ name: p.name })),
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
    let session: Session | null = null;
    const sessionOptions: SessionOptions = {
      graph: graph,
      components: this.#originalOptions.components,
      wasm: this.#originalOptions.wasm,
      capabilities: this.#originalOptions.capabilities,
      mocks: Object.keys(mocks).length > 0 ? mocks : undefined,

      beforeRun: async (node): Promise<void> => {
        if (this.#breakpoints.has(node)) {
          // Read when it is hit, as nothing crosses to the host unasked.
          const inputs = (await session?.readInputs(node)) ?? {};
          this.#pausedNode = node;
          this.#breakpointState = { node, inputs };
          this.#mode = 'paused';
          this.#pullSoon();
          this.#dispatch('state-changed');
          return new Promise<void>((resolve) => {
            this.#breakpointResolve = resolve;
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
      },

      onStreamItem: ({ node, port, items }): void => {
        if (this.#session !== session || !this.isObserved(node, port)) return;
        const resource = this.#resourceOf(node, port);
        if (resource === undefined) this.#recordItems(node, port, items);
        else for (const item of Array.from(items)) this.#recordHandle(node, port, item, resource);
      },

      onFutureValue: ({ node, port, value }): void => {
        if (this.#session !== session || !this.isObserved(node, port)) return;
        const resource = this.#resourceOf(node, port);
        if (resource === undefined) this.#record(node, port, value);
        else this.#recordHandle(node, port, value, resource);
      },
    };

    session = await Session.load(sessionOptions);
    this.#session = session;
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
    await this.#restoreInjections(graph);
    this.#collectCapabilities();
  }

  #collectCapabilities(): void {
    this.#capabilityInfo.clear();
    if (!this.#session) return;
    const capabilitiesFn = this.#originalOptions?.capabilities;
    for (const [nodeId, contract] of this.#session.contracts) {
      if (!contract.capabilities || contract.capabilities.length === 0) continue;
      const infos: CapabilityInfo[] = contract.capabilities.map((cap) => {
        // A capability is satisfied when the capabilities callback exists
        // and doesn't throw for this node. We can't cheaply test the return
        // value without instantiating, so mark satisfied when a callback is
        // provided — the session load would have thrown if it returned undefined.
        const satisfied = capabilitiesFn !== undefined;
        return {
          interface: cap.name,
          implements: cap.implements,
          satisfied,
        };
      });
      this.#capabilityInfo.set(nodeId, infos);
    }
  }

  async #restoreInjections(graph: GraphJson): Promise<void> {
    if (!this.#session) return;
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

    const faults = this.#session.graph.takeFaults() as FaultReport[];
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
        visual.phase = state.phase;
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
          this.#record(id, port.name, value);
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
    const message = 'The graph cannot be edited while it runs: stop it first';
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
        this.#compileDiagnostics = [];
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
        this.#compileDiagnostics = (diagnosticsOf(err) ?? []).filter(
          ({ severity }) => severity === 'error',
        );
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
