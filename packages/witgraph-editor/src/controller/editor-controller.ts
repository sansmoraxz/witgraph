import {
  type Contract,
  describeComponent,
  type PortValues,
  Session,
  type SessionOptions,
} from 'witgraph-loader';
import { NODE_HEADER_HEIGHT } from '../components/styles.ts';
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
  OutputSnapshot,
  OutputView,
  PortVisual,
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
const TICK_SLICE_MS = 100;

/** Waits for the next frame, so a run that never settles leaves the page time to draw. */
function nextFrame(): Promise<void> {
  return new Promise((resolve) =>
    typeof requestAnimationFrame === 'function'
      ? requestAnimationFrame(() => resolve())
      : setTimeout(resolve, 16),
  );
}
const MAX_HISTORY = 50;
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
  /** Value outputs only: the tick its latest value was recorded at... */
  tick: number;
  /** ...and that value as WAVE text, to tell a new value from it. */
  wave?: string;
  bytes: number;
}

/** The most edits undo goes back over. */
const MAX_UNDO = 50;

/**
 * About how many bytes `value` holds: its strings and buffers, through its
 * lists and records, and a little for the rest.
 */
function weight(value: unknown, depth = 0): number {
  if (typeof value === 'string') return 16 + value.length * 2;
  if (ArrayBuffer.isView(value)) return 16 + value.byteLength;
  if (typeof value !== 'object' || value === null) return 8;
  // Deeper than any WIT type nests in practice.
  if (depth > 16) return 16;
  let bytes = 16;
  for (const item of Array.isArray(value) ? value : Object.values(value)) {
    bytes += weight(item, depth + 1);
  }
  return bytes;
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
  #outputHistory = new Map<string, Map<string, OutputSnapshot[]>>();
  /** Per node and output, what it carried: see {@link observe}. */
  #observed = new Map<string, Map<string, Observation>>();
  #stopRequested = false;
  #islands: string[][] = [];
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
  get compiles(): boolean {
    return this.#compileError === null && this.#session !== null;
  }
  get lastRunTicks(): number {
    return this.#lastRunTicks;
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
   * The graph's execution islands, as it was compiled: read once per load,
   * since the scheduler cannot be asked while a tick holds it.
   */
  get islands(): readonly string[][] {
    return this.#islands;
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

  outputHistory(node: string, port: string): readonly OutputSnapshot[] {
    return this.#outputHistory.get(node)?.get(port) ?? [];
  }

  /**
   * What an output has carried: its latest value and those before it, as a
   * node's preview shows them. A Value output's latest is read from the
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
    if (kind === 'value' && this.#session) {
      // Read as WAVE text first: each read as a value is a new object, so
      // only the text tells whether it is still the value recorded.
      let wave: string | undefined;
      try {
        wave = this.#session.graph.readOutputWave(node, port);
      } catch {}
      if (wave !== undefined && wave !== seen?.wave) {
        // Mid-tick, the value so far is not recorded yet: show it last.
        try {
          view.latest = this.#session.graph.readOutput(node, port);
          view.recent = [...view.recent, view.latest];
        } catch {}
      }
    }
    return view;
  }

  /** What an output carried last: {@link observe}'s `latest`. */
  latestOutput(node: string, port: string): unknown {
    return this.observe(node, port).latest;
  }

  /** The record of what `node`'s `port` carried, made on first use. */
  #observation(node: string, port: string): Observation {
    const ports = this.#observed.get(node) ?? new Map<string, Observation>();
    this.#observed.set(node, ports);
    let seen = ports.get(port);
    if (!seen) {
      seen = { latest: undefined, recent: [], count: 0, pending: false, tick: -1, bytes: 0 };
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
    inputs: PortVisual[];
    outputs: PortVisual[];
  }[] {
    if (!this.#session || !this.#originalOptions) return [];
    const result: { id: string; index: number; inputs: PortVisual[]; outputs: PortVisual[] }[] = [];
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
      nodeVisual({ id, attrs: node.attrs, inputs, outputs, phase: 'pending', breakpoint: false }),
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
    for (const observed of [this.#outputHistory, this.#observed] as Map<string, unknown>[]) {
      const entry = observed.get(oldId);
      if (entry) {
        observed.delete(oldId);
        observed.set(newId, entry);
      }
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
    return port?.kind === 'value';
  }

  canConnect(
    fromNode: string,
    fromPort: string,
    toNode: string,
    toPort: string,
  ): { ok: boolean; reason?: string } {
    const srcNode = this.#nodes.get(fromNode);
    const tgtNode = this.#nodes.get(toNode);
    if (!srcNode) return { ok: false, reason: `unknown node '${fromNode}'` };
    if (!tgtNode) return { ok: false, reason: `unknown node '${toNode}'` };
    const srcPort = srcNode.outputs.find((p) => p.name === fromPort);
    const tgtPort = tgtNode.inputs.find((p) => p.name === toPort);
    if (!srcPort) return { ok: false, reason: `'${fromNode}' has no output '${fromPort}'` };
    if (!tgtPort) return { ok: false, reason: `'${toNode}' has no input '${toPort}'` };
    if (srcPort.kind !== tgtPort.kind)
      return { ok: false, reason: `kind mismatch: ${srcPort.kind} -> ${tgtPort.kind}` };
    const duplicate = this.#edges.some(
      (e) =>
        e.fromNode === fromNode &&
        e.fromPort === fromPort &&
        e.toNode === toNode &&
        e.toPort === toPort,
    );
    if (duplicate) return { ok: false, reason: 'connection already exists' };
    return { ok: true };
  }

  compatibleOutputsForInput(
    nodeId: string,
    portName: string,
  ): readonly { node: string; port: string; label: string; self: boolean }[] {
    const tgtNode = this.#nodes.get(nodeId);
    const tgtPort = tgtNode?.inputs.find((p) => p.name === portName);
    if (!tgtPort) return [];
    const result: { node: string; port: string; label: string; self: boolean }[] = [];
    for (const [id, n] of this.#nodes) {
      for (const p of n.outputs) {
        if (p.kind !== tgtPort.kind) continue;
        if (id === nodeId && tgtPort.kind !== 'value') continue;
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
    if (!this.#originalOptions) throw new Error('no session loaded');
    const graphJson = JSON.parse(json) as GraphJson;
    this.#graphJson = graphJson;
    this.#undoStack = [];
    this.#redoStack = [];
    this.#traceEvents = [];
    this.#outputHistory.clear();
    this.#observed.clear();
    this.#tickCount = 0;
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#selectedNode = null;
    this.#selectedEdge = null;
    await this.#reloadAfterEdit();
  }

  async importBundle(json: string): Promise<void> {
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
    const graphJson: GraphJson =
      typeof options.graph === 'string'
        ? (JSON.parse(options.graph) as GraphJson)
        : structuredClone(options.graph);
    this.#graphJson = graphJson;
    await this.#createSession(graphJson);
    this.#mode = 'idle';
    this.#dispatch('state-changed');
  }

  dispose(): void {
    this.#session?.dispose();
    this.#session = null;
    this.#islands = [];
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
    this.#outputHistory.clear();
    this.#observed.clear();
    this.#injectedValues.clear();
    this.#undoStack.length = 0;
    this.#redoStack.length = 0;
    this.#clipboard = null;
    this.#capabilityInfo.clear();
    this.#savedSnapshots = [];
    this.#compileError = null;
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#componentContracts.clear();
  }

  async reset(): Promise<void> {
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
    let result: Awaited<ReturnType<Session['tick']>>['result'];
    try {
      // Paused at a breakpoint, a tick ends only when resumed or stopped.
      const stop = Promise.race([stopped, slice.then(() => this.#paused())]);
      result = (await session.tick(stop)).result;
    } finally {
      this.#ticking = false;
      this.#stopResolve = null;
      clearTimeout(timer);
    }
    this.#tickCount++;
    const stop = this.#stopRequested;
    if (stop) this.#shutdown();
    this.#pollState();
    if (stop) return 'stopped';
    if (result === 'aborted') return 'aborted';
    return result === 'idle' ? 'idle' : 'progress';
  }

  /** Resolves once no node is held at a breakpoint. */
  async #paused(): Promise<void> {
    while (this.#breakpointResolve && !this.#stopRequested) await nextFrame();
  }

  /** Stops every island: what is still running ends, streams included. */
  #shutdown(): void {
    try {
      this.#session?.graph.shutdown();
    } catch {}
  }

  /** Islands a stop cancelled run again on their latched inputs. */
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
    const cmd = this.#undoStack.pop();
    if (!cmd) return;
    cmd.undo();
    this.#redoStack.push(cmd);
    this.#dispatch('state-changed');
  }

  redo(): void {
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

  readOutput(node: string, port: string): unknown {
    if (!this.#session) throw new Error('no session loaded');
    return this.#session.graph.readOutput(node, port);
  }

  readOutputWave(node: string, port: string): string | undefined {
    if (!this.#session) throw new Error('no session loaded');
    return this.#session.graph.readOutputWave(node, port);
  }

  #injectedValues = new Map<string, Map<string, string>>();

  injectValue(node: string, port: string, wave: string): void {
    if (!this.#session) throw new Error('no session loaded');
    this.#session.graph.injectWave(node, port, wave);
    if (!this.#injectedValues.has(node)) this.#injectedValues.set(node, new Map());
    this.#injectedValues.get(node)!.set(port, wave);
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === node);
    if (graphNode) {
      if (!graphNode.attrs) graphNode.attrs = {};
      graphNode.attrs[`inject.${port}`] = wave;
    }
    this.#dispatch('state-changed');
  }

  clearInput(node: string, port: string): void {
    if (!this.#session) throw new Error('no session loaded');
    this.#session.graph.clearInput(node, port);
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
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    if (!graphNode.attrs) graphNode.attrs = {};
    graphNode.attrs[`mock.${port}`] = wave;
    if (this.isMocked(nodeId)) void this.#reloadAfterEdit();
    else this.#dispatch('state-changed');
  }

  clearMockOutput(nodeId: string, port: string): void {
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

  saveSnapshot(name?: string): void {
    if (!this.#session) throw new Error('no session loaded');
    const data = this.#session.graph.snapshot();
    const snapName = name ?? `snap-${this.#savedSnapshots.length + 1}`;
    this.#savedSnapshots.push({ name: snapName, data, timestamp: Date.now() });
    this.#dispatch('state-changed');
  }

  restoreNamedSnapshot(name: string): void {
    const snap = this.#savedSnapshots.find((s) => s.name === name);
    if (!snap) throw new Error(`no snapshot named "${name}"`);
    if (!this.#session) throw new Error('no session loaded');
    this.#session.graph.restore(snap.data);
    this.#pollState();
  }

  deleteSnapshot(name: string): void {
    this.#savedSnapshots = this.#savedSnapshots.filter((s) => s.name !== name);
    this.#dispatch('state-changed');
  }

  // -- Selection --------------------------------------------------------------

  select(nodeId: string | null): void {
    this.#selectedNode = nodeId;
    if (nodeId !== null) this.#selectedEdge = null;
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
    this.#outputHistory.delete(id);
    this.#observed.delete(id);
    this.#edges = this.#edges.filter((e) => e.fromNode !== id && e.toNode !== id);
    if (this.#selectedNode === id) this.#selectedNode = null;
    void this.#reloadAfterEdit();
  }

  addConnection(id: string, from: GraphJsonPortRef, to: GraphJsonPortRef, feedback = false): void {
    if (!this.#graphJson) throw new Error('no graph loaded');
    const check = this.canConnect(from.node, from.port, to.node, to.port);
    if (!check.ok) {
      this.#pushError(`Cannot connect: ${check.reason}`);
      return;
    }
    if (feedback && !this.canBeFeedback(from.node, from.port)) {
      feedback = false;
      this.#pushError('Feedback downgraded: only Value ports support feedback');
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

  snapshot(): string {
    if (!this.#session) throw new Error('no session loaded');
    return this.#session.graph.snapshot();
  }

  restore(snapshot: string): void {
    if (!this.#session) throw new Error('no session loaded');
    this.#session.graph.restore(snapshot);
    this.#pollState();
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

      beforeRun: (node, inputs): void | Promise<void> => {
        if (this.#breakpoints.has(node)) {
          this.#pausedNode = node;
          this.#breakpointState = { node, inputs: inputs as Record<string, unknown> };
          this.#mode = 'paused';
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

      afterRun: (): void => {},

      onStreamItem: ({ node, port, items }): void => {
        if (this.#session === session) this.#recordItems(node, port, items);
      },

      onFutureValue: ({ node, port, value }): void => {
        if (this.#session === session) this.#record(node, port, value);
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
    this.#islands = session.graph.islands() as string[][];
    this.#dirty = false;
    this.#buildVisuals();
    this.#restoreInjections(graph);
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

  #restoreInjections(graph: GraphJson): void {
    if (!this.#session) return;
    for (const node of graph.nodes) {
      if (!node.attrs) continue;
      for (const [key, value] of Object.entries(node.attrs)) {
        if (!key.startsWith('inject.')) continue;
        const port = key.slice('inject.'.length);
        try {
          this.#session.graph.injectWave(node.id, port, value);
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
      for (const member of fault.members) {
        const node = this.#nodes.get(member);
        if (node) {
          node.fault = fault.fault;
          if (fault.culprit) node.culprit = fault.culprit;
        }
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

    // Record output history
    for (const [id, visual] of this.#nodes) {
      for (const port of visual.outputs) {
        if (port.kind !== 'value') continue;
        try {
          const seen = this.#observed.get(id)?.get(port.name);
          if (seen?.tick !== this.#tickCount) {
            const wave = this.#session.graph.readOutputWave(id, port.name);
            const value = this.#session.graph.readOutput(id, port.name);
            if (wave !== undefined && value !== undefined) {
              this.#record(id, port.name, value);
              const recorded = this.#observation(id, port.name);
              recorded.tick = this.#tickCount;
              recorded.wave = wave;
            }
          }
        } catch {}
        try {
          const wave = this.#session.graph.readOutputWave(id, port.name);
          if (wave !== undefined) {
            if (!this.#outputHistory.has(id)) {
              this.#outputHistory.set(id, new Map());
            }
            const nodeHistory = this.#outputHistory.get(id)!;
            if (!nodeHistory.has(port.name)) {
              nodeHistory.set(port.name, []);
            }
            const portHistory = nodeHistory.get(port.name)!;
            const last = portHistory[portHistory.length - 1];
            if (!last || last.wave !== wave || last.tick !== this.#tickCount) {
              portHistory.push({ tick: this.#tickCount, wave });
              if (portHistory.length > MAX_HISTORY) portHistory.shift();
            }
          }
        } catch {}
      }
    }

    this.#dispatch('state-changed');
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
