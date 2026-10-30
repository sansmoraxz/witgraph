import { Session, type SessionOptions } from 'witgraph-loader';
import type {
  EditorMode,
  NodeVisual,
  EdgeVisual,
  PortVisual,
  TraceEntry,
  FaultReport,
  NodeState,
  EditorLoadOptions,
  EditorCommand,
  GraphJson,
  GraphJsonNode,
  GraphJsonConnection,
  GraphJsonPortRef,
  BreakpointState,
  OutputSnapshot,
} from './types.ts';
import { autoLayout, type LayoutNode, type LayoutEdge } from './layout.ts';
import { NODE_WIDTH, computeNodeHeight } from '../components/styles.ts';

const MAX_TRACE_ENTRIES = 1000;
const MAX_TICKS_PER_RUN = 200;
const MAX_HISTORY = 50;

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
  #lastRunDurationMs: number | null = null;
  #lastRunTicks = 0;

  #undoStack: EditorCommand[] = [];
  #redoStack: EditorCommand[] = [];
  #clipboard: { nodes: GraphJsonNode[]; edges: GraphJsonConnection[] } | null = null;
  #compileError: string | null = null;
  #tickDelayMs = 0;

  // -- Getters ----------------------------------------------------------------

  get mode(): EditorMode { return this.#mode; }
  get nodes(): ReadonlyMap<string, NodeVisual> { return this.#nodes; }
  get edges(): readonly EdgeVisual[] { return this.#edges; }
  get selectedNode(): string | null { return this.#selectedNode; }
  get selectedEdge(): string | null { return this.#selectedEdge; }
  get traceEvents(): readonly TraceEntry[] { return this.#traceEvents; }
  get graphJson(): object { return this.#graphJson ?? {}; }
  get pausedNode(): string | null { return this.#pausedNode; }
  get breakpointState(): BreakpointState | null { return this.#breakpointState; }
  get lastRunDurationMs(): number | null { return this.#lastRunDurationMs; }
  get compileError(): string | null { return this.#compileError; }
  get compiles(): boolean { return this.#compileError === null && this.#session !== null; }
  get lastRunTicks(): number { return this.#lastRunTicks; }

  get tickDelayMs(): number { return this.#tickDelayMs; }
  set tickDelayMs(ms: number) { this.#tickDelayMs = Math.max(0, ms); }

  get islands(): readonly string[][] {
    if (!this.#session) return [];
    try {
      return this.#session.graph.islands() as string[][];
    } catch {
      return [];
    }
  }
  get canUndo(): boolean { return this.#undoStack.length > 0; }
  get canRedo(): boolean { return this.#redoStack.length > 0; }
  get hasClipboard(): boolean { return this.#clipboard !== null; }

  outputHistory(node: string, port: string): readonly OutputSnapshot[] {
    return this.#outputHistory.get(node)?.get(port) ?? [];
  }

  get componentSources(): readonly { id: string; index: number; inputs: PortVisual[]; outputs: PortVisual[] }[] {
    if (!this.#session || !this.#originalOptions) return [];
    const result: { id: string; index: number; inputs: PortVisual[]; outputs: PortVisual[] }[] = [];
    const seen = new Set<number>();

    for (const [nodeId, visual] of this.#nodes) {
      const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
      if (!graphNode) continue;
      const compIdx = this.#graphJson?.components.findIndex((c) => {
        const ref = graphNode.component as Record<string, unknown>;
        const candidate = c as Record<string, unknown>;
        return EditorController.#shallowEqual(candidate, ref)
          || EditorController.#shallowEqual(candidate, ref, 'content_hash');
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

    // Also include components not yet used by any node
    for (let i = 0; i < this.#originalOptions.components.length; i++) {
      if (seen.has(i)) continue;
      result.push({
        id: this.#originalOptions.components[i].id,
        index: i,
        inputs: [],
        outputs: [],
      });
    }

    return result;
  }

  static #shallowEqual(a: Record<string, unknown>, b: Record<string, unknown>, ...ignore: string[]): boolean {
    const skip = new Set(ignore);
    const keysA = Object.keys(a).filter((k) => !skip.has(k));
    const keysB = Object.keys(b).filter((k) => !skip.has(k));
    if (keysA.length !== keysB.length) return false;
    return keysA.every((k) => b[k] === a[k]);
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
    this.#nodes.set(id, {
      id, x, y, width: NODE_WIDTH, height: computeNodeHeight(inputs.length, outputs.length),
      label: id, phase: 'pending', breakpoint: false, expanded: false,
      inputs: [...inputs], outputs: [...outputs],
    });
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
    this.#dispatch('state-changed');
  }

  removeNodeAttr(nodeId: string, key: string): void {
    const graphNode = this.#graphJson?.nodes.find((n) => n.id === nodeId);
    if (graphNode?.attrs) {
      delete graphNode.attrs[key];
    }
    this.#dispatch('state-changed');
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

  canConnect(fromNode: string, fromPort: string, toNode: string, toPort: string): { ok: boolean; reason?: string } {
    const srcNode = this.#nodes.get(fromNode);
    const tgtNode = this.#nodes.get(toNode);
    if (!srcNode) return { ok: false, reason: `unknown node '${fromNode}'` };
    if (!tgtNode) return { ok: false, reason: `unknown node '${toNode}'` };
    const srcPort = srcNode.outputs.find((p) => p.name === fromPort);
    const tgtPort = tgtNode.inputs.find((p) => p.name === toPort);
    if (!srcPort) return { ok: false, reason: `'${fromNode}' has no output '${fromPort}'` };
    if (!tgtPort) return { ok: false, reason: `'${toNode}' has no input '${toPort}'` };
    if (srcPort.kind !== tgtPort.kind) return { ok: false, reason: `kind mismatch: ${srcPort.kind} -> ${tgtPort.kind}` };
    const duplicate = this.#edges.some(
      (e) => e.fromNode === fromNode && e.fromPort === fromPort && e.toNode === toNode && e.toPort === toPort,
    );
    if (duplicate) return { ok: false, reason: 'connection already exists' };
    return { ok: true };
  }

  compatibleOutputsForInput(nodeId: string, portName: string): readonly { node: string; port: string; label: string; self: boolean }[] {
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

  connectInput(nodeId: string, portName: string, fromNode: string, fromPort: string, feedback = false): void {
    const existing = this.connectionForInput(nodeId, portName);
    if (existing) this.removeConnection(existing.id);
    const id = `c-${fromNode}-${fromPort}-${nodeId}-${portName}`;
    this.addConnection(id, { node: fromNode, port: fromPort }, { node: nodeId, port: portName }, feedback);
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
    this.#tickCount = 0;
    this.#lastRunDurationMs = null;
    this.#lastRunTicks = 0;
    this.#selectedNode = null;
    this.#selectedEdge = null;
    await this.#reloadAfterEdit();
  }

  async importBundle(json: string): Promise<void> {
    const prev = this.#originalOptions;
    const bundle = JSON.parse(json) as { version: number; graph: GraphJson; components: { id: string; wit?: string; bytes: string }[] };
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
        if (!file) { resolve(); return; }
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
    this.#injectedValues.clear();
    this.#undoStack.length = 0;
    this.#redoStack.length = 0;
    this.#clipboard = null;
  }

  async reset(): Promise<void> {
    const options = this.#originalOptions;
    this.dispose();
    if (options) await this.load(options);
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
    this.#dispatch('state-changed');

    const startTime = performance.now();
    let ticks = 0;
    const stopPromise = new Promise<void>((resolve) => {
      this.#stopResolve = resolve;
    });

    try {
      let done = false;
      while (!done) {
        const tick = await this.#session!.tick(stopPromise);
        this.#tickCount++;
        ticks++;
        this.#pollState();

        switch (tick.result) {
          case 'progress':
          case 'step-limit':
            if (ticks >= MAX_TICKS_PER_RUN) {
              done = true;
              this.#pushError(`Run stopped after ${MAX_TICKS_PER_RUN} ticks (limit reached)`);
              this.#mode = 'idle';
            }
            break;
          case 'idle':
          case 'interrupted':
            done = true;
            if ((this.#mode as EditorMode) !== 'paused') {
              this.#mode = 'idle';
            }
            break;
          case 'aborted':
            done = true;
            this.#mode = 'error';
            break;
        }
      }
    } catch (err) {
      this.#mode = 'error';
      const msg = err instanceof Error ? err.message : String(err);
      this.#pushError(`Run failed: ${msg}`);
    } finally {
      this.#lastRunDurationMs = performance.now() - startTime;
      this.#lastRunTicks = ticks;
      this.#stopResolve = null;
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
    this.#dispatch('state-changed');

    try {
      await this.#session.tick();
      this.#tickCount++;
      this.#pollState();
    } catch {
      this.#mode = 'error';
    }

    if ((this.#mode as EditorMode) !== 'paused') {
      this.#mode = 'idle';
    }
    this.#dispatch('state-changed');
  }

  stop(): void {
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

  toggleExpanded(nodeId: string): void {
    const visual = this.#nodes.get(nodeId);
    if (visual) {
      visual.expanded = !visual.expanded;
      this.#dispatch('state-changed');
    }
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
    if (this.#undoStack.length > 50) this.#undoStack.shift();
    this.#redoStack.length = 0;
  }

  // -- Copy / Paste -----------------------------------------------------------

  copySelected(): void {
    const nodeId = this.#selectedNode;
    if (!nodeId || !this.#graphJson) return;
    const graphNode = this.#graphJson.nodes.find((n) => n.id === nodeId);
    if (!graphNode) return;
    this.#clipboard = {
      nodes: [structuredClone(graphNode)],
      edges: [],
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
        const graphComp = this.#graphJson!.components[s.index] as Record<string, unknown> | undefined;
        return graphComp && EditorController.#shallowEqual(graphComp, compRef as Record<string, unknown>);
      });
      if (!source) continue;
      const newNode: GraphJsonNode = {
        ...structuredClone(node),
        id: newId,
        attrs: { 'editor.x': String(x), 'editor.y': String(y) },
      };
      this.#graphJson.nodes.push(newNode);
      this.#nodes.set(newId, {
        id: newId, x, y, width: NODE_WIDTH,
        height: computeNodeHeight(source.inputs.length, source.outputs.length),
        label: newNode.label ?? newId, phase: 'pending', breakpoint: false, expanded: false,
        inputs: [...source.inputs], outputs: [...source.outputs],
      });
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
    this.#nodes.set(id, {
      id, x, y, width: NODE_WIDTH, height: computeNodeHeight(0, 0),
      label: id, phase: 'pending', breakpoint: false, expanded: false, inputs: [], outputs: [],
    });
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
    if (this.#undoStack.length > 50) this.#undoStack.shift();
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
      id, from: { node: from.node, port: from.port }, to: { node: to.node, port: to.port },
      ...(feedback ? { feedback: true } : {}),
    };
    this.#graphJson.connections.push(conn);
    this.#edges.push({
      id, fromNode: from.node, fromPort: from.port,
      toNode: to.node, toPort: to.port, feedback,
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
    if (this.#undoStack.length > 50) this.#undoStack.shift();
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

  exportGraph(): string { return JSON.stringify(this.#graphJson, null, 2); }

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
      id: n.id, width: n.width, height: n.height,
      inputs: n.inputs.map((p) => ({ name: p.name })),
      outputs: n.outputs.map((p) => ({ name: p.name })),
    }));
    const layoutEdges: LayoutEdge[] = this.#edges.map((e) => ({
      id: e.id, fromNode: e.fromNode, fromPort: e.fromPort,
      toNode: e.toNode, toPort: e.toPort,
    }));
    const positions = await autoLayout(layoutNodes, layoutEdges);
    for (const [id, pos] of positions) this.moveNode(id, pos.x, pos.y);
    this.#dispatch('state-changed');
  }

  // -- Internal ---------------------------------------------------------------

  async #createSession(graph: GraphJson): Promise<void> {
    if (!this.#originalOptions) throw new Error('no options');

    const sessionOptions: SessionOptions = {
      graph: graph,
      components: this.#originalOptions.components,
      wasm: this.#originalOptions.wasm,
      capabilities: this.#originalOptions.capabilities,
      mocks: this.#originalOptions.mocks,

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
        if (this.#tickDelayMs > 0) {
          const visual = this.#nodes.get(node);
          if (visual) { visual.phase = 'running'; this.#dispatch('state-changed'); }
          return new Promise<void>((resolve) => setTimeout(resolve, this.#tickDelayMs));
        }
      },

      afterRun: (): void => {},

      onStreamItem: (): void => {
        this.#dispatch('output-changed');
      },
    };

    this.#session = await Session.load(sessionOptions);
    this.#dirty = false;
    this.#buildVisuals();
    this.#restoreInjections(graph);
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
      const inputs: PortVisual[] = (contract?.inputs ?? []).map((p) => ({
        name: p.name, kind: p.kind, typeDisplay: p.type_display,
        optional: p.optional, docs: p.docs,
      }));
      const outputs: PortVisual[] = (contract?.outputs ?? []).map((p) => ({
        name: p.name, kind: p.kind, typeDisplay: p.type_display,
        optional: p.optional, docs: p.docs,
      }));

      const x = parseFloat(graphNode.attrs?.['editor.x'] ?? '') || 0;
      const y = parseFloat(graphNode.attrs?.['editor.y'] ?? '') || 0;
      const width = NODE_WIDTH;
      const height = computeNodeHeight(inputs.length, outputs.length);

      const nodeVisual: NodeVisual = {
        id: graphNode.id, x, y, width, height,
        label: graphNode.label ?? graphNode.id,
        phase: 'idle', breakpoint: this.#breakpoints.has(graphNode.id), expanded: false,
        inputs, outputs,
      };

      try {
        const state = this.#session.graph.nodeState(graphNode.id) as NodeState;
        nodeVisual.phase = state.phase;
        if (state.fault) nodeVisual.fault = state.fault;
        if (state.culprit) nodeVisual.culprit = state.culprit;
      } catch {}

      this.#nodes.set(graphNode.id, nodeVisual);
    }

    for (const conn of this.#graphJson.connections) {
      this.#edges.push({
        id: conn.id, fromNode: conn.from.node, fromPort: conn.from.port,
        toNode: conn.to.node, toPort: conn.to.port,
        feedback: conn.feedback ?? false,
      });
    }
  }

  #pollState(): void {
    if (!this.#session) return;

    const now = performance.now();
    const trace = (this.#session.graph.takeTrace() as TraceEntry[]).map(
      (e) => ({ ...e, timestamp: e.timestamp ?? now }),
    );
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
    }

    // Record output history
    for (const [id, visual] of this.#nodes) {
      for (const port of visual.outputs) {
        if (port.kind !== 'value') continue;
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
  #dispatch(type: string): void {
    if (type === 'state-changed') {
      if (this.#dispatchQueued) return;
      this.#dispatchQueued = true;
      queueMicrotask(() => {
        this.#dispatchQueued = false;
        this.dispatchEvent(new CustomEvent(type));
      });
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
  async #reloadAfterEdit(): Promise<void> {
    this.#dirty = true;
    if (this.#reloading) return;
    this.#reloading = true;
    const hadError = this.#compileError !== null;
    try {
      await this.#reload();
      this.#compileError = null;
      if (hadError) {
        this.#traceEvents.push({ event: 'compile-ok', timestamp: performance.now(), message: 'Graph compiles successfully' });
        this.#dispatch('trace-event');
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      this.#compileError = msg;
      this.#pushError(`Graph reload failed: ${msg}`);
    } finally {
      this.#reloading = false;
    }
    this.#dispatch('state-changed');
  }
}
