import type { Diagnostic } from 'witgraph-loader';
import type { EditorController } from '../controller/editor-controller.ts';
import type { EdgeVisual, NodePreview, NodeVisual } from '../controller/types.ts';
import { makeDraggable } from '../util/drag.ts';
import { outputText } from '../util/output-text.ts';
import { edgePath, hasField, portOffset } from '../util/svg.ts';
import { viewerFor } from '../viewers/registry.ts';
import type { ViewerInstance } from '../viewers/types.ts';
import { ContextMenu, type MenuItem } from './context-menu.ts';
import { adoptStyles } from './shared-styles.ts';
import {
  FIELD_ROW_HEIGHT,
  MIN_NODE_WIDTH,
  MIN_PREVIEW_HEIGHT,
  NODE_HEADER_HEIGHT,
  PORT_RADIUS,
} from './styles.ts';

const SVG_NS = 'http://www.w3.org/2000/svg';

const SVG_CSS = `
  :host { display: block; width: 100%; height: 100%; overflow: hidden; background-color: var(--wg-bg);
    --_node-radius: var(--wg-node-radius, 8px);
    --_port-radius: var(--wg-port-radius, 6px); }
  svg { width: 100%; height: 100%; user-select: none; -webkit-user-select: none; }
  .node-group text { user-select: none; }
  .port-label, .node-label { pointer-events: none; }

  /* SVG defs — gradient and pattern driven by theme */
  #dot-grid circle { fill: var(--wg-surface); }
  #node-gradient stop:first-child { stop-color: var(--wg-node-grad-top); }
  #node-gradient stop:last-child { stop-color: var(--wg-node-grad-bottom); }

  /* Node styling — solid with depth */
  .node-group { cursor: grab; filter: drop-shadow(0 2px 4px rgba(0,0,0,0.3)); transition: filter 0.15s; }
  .node-group.selected { filter: drop-shadow(0 0 8px color-mix(in srgb, var(--wg-primary) 50%, transparent)) drop-shadow(0 2px 4px rgba(0,0,0,0.3)); }
  .node-body { fill: url(#node-gradient); stroke: var(--wg-surface-hover); stroke-width: 1; rx: var(--_node-radius); }
  /* Hovered: part of the way to selected, by the outline alone, so a previewing node has it too.
     Before the rules below, which say more, so win at the same specificity. */
  .node-group:hover .node-body { stroke: color-mix(in srgb, var(--wg-primary) 55%, var(--wg-surface-hover)); stroke-width: 1.5; }
  .node-group.selected .node-body { stroke: var(--wg-primary); stroke-width: 2; }
  .node-group.paused-here .node-body { stroke: var(--wg-warning); stroke-width: 3; stroke-dasharray: 6 3; }
  .node-group.has-warning .node-body { stroke: var(--wg-warning); stroke-width: 2; }
  .node-group.has-error .node-body { stroke: var(--wg-error); stroke-width: 2; }
  .edge.has-warning { stroke: var(--wg-warning); }
  .edge.has-error { stroke: var(--wg-error); }
  .node-header { rx: var(--_node-radius); }
  .node-header.pending { fill: var(--wg-phase-pending); }
  .node-header.running { fill: var(--wg-phase-running); }
  .node-header.idle { fill: var(--wg-phase-idle); }
  .node-header.faulted { fill: var(--wg-phase-faulted); }
  .node-header.cancelled { fill: var(--wg-phase-cancelled); }
  .node-label { fill: var(--wg-bg); font-weight: 700; font-size: 11px; text-anchor: middle;
    dominant-baseline: central; pointer-events: none; letter-spacing: 0.02em;
    overflow: hidden; text-overflow: ellipsis; }

  /* Ports — larger with ring */
  .port { cursor: pointer; stroke: var(--wg-bg); stroke-width: 2; transition: r 0.1s, stroke 0.1s; }
  .port-value { fill: var(--wg-port-value); }
  .port-stream { fill: var(--wg-port-stream); }
  .port-future { fill: var(--wg-port-future); }
  .port-handle { fill: var(--wg-port-handle); }
  .port:hover { r: 8; stroke: var(--wg-text); stroke-width: 2; }
  .port-drop-target { r: 9; stroke: var(--wg-success); stroke-width: 2.5; }
  .port-label { fill: var(--wg-text); font-size: 10px; pointer-events: none; font-weight: 500; }

  /* Edges — colored by type */
  .edge { fill: none; stroke-width: 2.5; stroke-linecap: round; transition: stroke-width 0.1s, filter 0.15s; }
  .edge-value { stroke: var(--wg-port-value); }
  .edge-stream { stroke: var(--wg-port-stream); }
  .edge-future { stroke: var(--wg-port-future); }
  .edge-handle { stroke: var(--wg-port-handle); }
  .edge-default { stroke: var(--wg-text-muted); }
  .edge-feedback { stroke-dasharray: 8 4; }
  .edge.selected { stroke-width: 3.5; filter: drop-shadow(0 0 4px color-mix(in srgb, var(--wg-primary) 40%, transparent)); }
  .edge-hit { fill: none; stroke: transparent; stroke-width: 14; pointer-events: stroke; cursor: pointer; }
  .edge-hit:hover + .edge { stroke-width: 3; filter: drop-shadow(0 0 3px color-mix(in srgb, var(--wg-primary) 30%, transparent)); }

  /* Breakpoint — pin icon */
  .breakpoint { fill: var(--wg-error); filter: drop-shadow(0 0 3px color-mix(in srgb, var(--wg-error) 50%, transparent)); }

  /* Temp edge for connection drawing */
  .temp-edge { fill: none; stroke: var(--wg-primary); stroke-width: 2; stroke-dasharray: 6 4;
    pointer-events: none; opacity: 0.8; }

  /* Preview region: a node's output, shown by its viewer. A filter would
     re-rasterize the node on every frame, so a previewing node has none. */
  .node-group.has-preview, .node-group.has-preview.selected { filter: none; }
  .node-preview-host { box-sizing: border-box; width: 100%; height: 100%; padding: 0 6px 6px;
    overflow: hidden; }
  .preview-image { display: block; width: 100%; height: 100%; object-fit: contain;
    image-rendering: pixelated; background: var(--wg-bg); border: 1px solid var(--wg-surface-hover);
    border-radius: 4px; box-sizing: border-box; }
  .preview-text { box-sizing: border-box; width: 100%; height: 100%; margin: 0; padding: 4px;
    overflow: hidden; white-space: pre-wrap; word-break: break-all; font: 10px ui-monospace, monospace;
    color: var(--wg-text); background: var(--wg-bg); border: 1px solid var(--wg-surface-hover);
    border-radius: 4px; }
  .preview-text.pending { color: var(--wg-text-muted); font-style: italic; }
  .preview-series { box-sizing: border-box; width: 100%; height: 100%; display: flex;
    flex-direction: column; gap: 2px; padding: 4px; background: var(--wg-bg);
    border: 1px solid var(--wg-surface-hover); border-radius: 4px; }
  .preview-series svg { flex: 1; min-height: 0; width: 100%; overflow: visible; }
  .series-line { fill: none; stroke: var(--wg-primary); stroke-width: 1.5;
    vector-effect: non-scaling-stroke; stroke-linejoin: round; }
  .series-area { fill: color-mix(in srgb, var(--wg-primary) 15%, transparent); stroke: none; }
  .preview-series-label { font: 10px ui-monospace, monospace; color: var(--wg-text-dim);
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }

  /* Fields: a Value input's value on its node, typed in or, when a connection feeds it, shown. */
  .node-field-row { box-sizing: border-box; display: flex; align-items: center; gap: 6px;
    width: 100%; height: 100%; font-size: 10px; }
  .node-field-name { flex: 0 0 auto; width: var(--_name-width); max-width: 45%; color: var(--wg-text); font-weight: 500;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .node-field-input, .node-field-value { flex: 1; min-width: 0; box-sizing: border-box; height: 100%;
    margin: 0; padding: 0 6px; font: 10px ui-monospace, monospace; border-radius: 4px;
    border: 1px solid var(--wg-surface-hover); }
  .node-field-input { background: var(--wg-bg); color: var(--wg-text); outline: none; cursor: text;
    user-select: text; -webkit-user-select: text; }
  .node-field-input:focus { border-color: var(--wg-primary); }
  .node-field-input::placeholder { color: var(--wg-text-muted); }
  .node-field-input.invalid { border-color: var(--wg-error); }
  .node-field-value { display: block; line-height: ${FIELD_ROW_HEIGHT - 6}px; color: var(--wg-text-dim);
    border-style: dashed; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }

  /* Resize handle: a corner to grab, and a grip drawn inside the node's rounded one */
  .node-resize { fill: transparent; cursor: nwse-resize; }
  .node-resize-grip { fill: none; stroke: var(--wg-text-muted); stroke-width: 1.25; stroke-linecap: round;
    opacity: 0; pointer-events: none; transition: opacity 0.15s, stroke 0.12s; }
  .node-group:hover .node-resize-grip, .node-resize.active + .node-resize-grip { opacity: 1; }
  .node-resize:hover + .node-resize-grip, .node-resize.active + .node-resize-grip { stroke: var(--wg-text-dim); }
`;

/** What a node's group was drawn for: a change redraws it from scratch. */
function layoutKey(node: NodeVisual): string {
  const ports = (list: NodeVisual['inputs']) => list.map((p) => [p.name, p.kind]);
  return JSON.stringify([
    node.width,
    node.height,
    node.portsHeight,
    ports(node.inputs),
    ports(node.outputs),
    node.preview?.port,
    node.preview?.viewer,
  ]);
}

/** Where a node's field rows start: past its input ports and their rings. */
const FIELD_X = PORT_RADIUS + 4;

/** How wide a node's fields are, from {@link FIELD_X} to just short of its right edge. */
function fieldWidth(nodeWidth: number): number {
  return nodeWidth - FIELD_X - 6;
}

/** Where a node's ports band ends: see {@link portOffset}. */
function portsBottom(node: NodeVisual): number {
  return NODE_HEADER_HEIGHT + node.portsHeight;
}

const PHASES = ['pending', 'running', 'idle', 'faulted', 'cancelled'] as const;

export class WitgraphCanvas extends HTMLElement {
  #controller: EditorController | null = null;
  #svg!: SVGSVGElement;
  #viewport!: SVGGElement;
  #edgesGroup!: SVGGElement;
  #nodesGroup!: SVGGElement;

  #nodeEls = new Map<string, SVGGElement>();
  #edgeEls = new Map<string, { visible: SVGPathElement; hit: SVGPathElement }>();
  #dragCleanups = new Map<string, () => void>();
  /** Each previewing node's viewer, with the output it shows. */
  #previews = new Map<string, { port: string; viewer: ViewerInstance }>();
  /** What each field showing a connection's value was filled from, so a redraw reads it only when it is new. */
  #fieldShown = new WeakMap<Element, { value: unknown; text: string }>();
  /** The node being resized: left alone by redraws until the drag ends. */
  #resizing: string | null = null;

  #panX = 0;
  #panY = 0;
  #scale = 1;
  #didDrag = false;
  #connectFrom: { node: string; port: string; x: number; y: number } | null = null;
  #contextMenu = new ContextMenu();
  #lastClickX = 0;
  #lastClickY = 0;

  #onStateChanged = () => this.#render();
  #onOutputChanged = () => this.#updatePreviews();

  connectedCallback(): void {
    // Moved in the page: listen again, and draw what was removed.
    if (this.shadowRoot) {
      this.#listen();
      return;
    }
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, SVG_CSS);

    this.#svg = document.createElementNS(SVG_NS, 'svg');

    // Defs: dot grid pattern + node body gradient
    const defs = document.createElementNS(SVG_NS, 'defs');
    const pattern = document.createElementNS(SVG_NS, 'pattern');
    pattern.setAttribute('id', 'dot-grid');
    pattern.setAttribute('width', '20');
    pattern.setAttribute('height', '20');
    pattern.setAttribute('patternUnits', 'userSpaceOnUse');
    const dot = document.createElementNS(SVG_NS, 'circle');
    dot.setAttribute('cx', '10');
    dot.setAttribute('cy', '10');
    dot.setAttribute('r', '1');
    pattern.appendChild(dot);
    defs.appendChild(pattern);

    const grad = document.createElementNS(SVG_NS, 'linearGradient');
    grad.setAttribute('id', 'node-gradient');
    grad.setAttribute('x1', '0');
    grad.setAttribute('y1', '0');
    grad.setAttribute('x2', '0');
    grad.setAttribute('y2', '1');
    const stop1 = document.createElementNS(SVG_NS, 'stop');
    stop1.setAttribute('offset', '0%');
    const stop2 = document.createElementNS(SVG_NS, 'stop');
    stop2.setAttribute('offset', '100%');
    grad.append(stop1, stop2);
    defs.appendChild(grad);
    this.#svg.appendChild(defs);

    // Background rect with dot grid
    const bg = document.createElementNS(SVG_NS, 'rect');
    bg.setAttribute('width', '100%');
    bg.setAttribute('height', '100%');
    bg.setAttribute('fill', 'url(#dot-grid)');
    this.#svg.appendChild(bg);

    this.#viewport = document.createElementNS(SVG_NS, 'g');
    this.#viewport.classList.add('viewport');
    this.#edgesGroup = document.createElementNS(SVG_NS, 'g');
    this.#edgesGroup.classList.add('edges');
    this.#nodesGroup = document.createElementNS(SVG_NS, 'g');
    this.#nodesGroup.classList.add('nodes');

    this.#viewport.appendChild(this.#edgesGroup);
    this.#viewport.appendChild(this.#nodesGroup);
    this.#svg.appendChild(this.#viewport);
    shadow.appendChild(this.#svg);

    this.#svg.addEventListener('pointerdown', (e) => this.#onBgPointerDown(e));
    this.#svg.addEventListener('wheel', (e) => this.#onWheel(e), { passive: false });
    this.addEventListener('dragover', (e) => e.preventDefault());
    this.addEventListener('drop', (e) => this.#onDrop(e));
    this.setAttribute('tabindex', '0');
    this.addEventListener('keydown', (e) => this.#onKeyDown(e));
    this.#svg.addEventListener('contextmenu', (e) => this.#onContextMenu(e));

    this.#listen();
  }

  disconnectedCallback(): void {
    this.#unlisten();
    // Out of the page, nothing is drawn: the nodes and their viewers go,
    // and come back when it is connected again.
    for (const id of [...this.#nodeEls.keys()]) this.#removeNodeGroup(id);
  }

  set controller(ctrl: EditorController) {
    this.#unlisten();
    this.#controller = ctrl;
    if (this.isConnected && this.shadowRoot) this.#listen();
  }

  /** Follows the controller's changes, and draws its graph as it is now. */
  #listen(): void {
    if (!this.#controller) return;
    this.#controller.addEventListener('state-changed', this.#onStateChanged);
    this.#controller.addEventListener('output-changed', this.#onOutputChanged);
    this.#render();
  }

  #unlisten(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller?.removeEventListener('output-changed', this.#onOutputChanged);
  }

  get controller(): EditorController | null {
    return this.#controller;
  }

  #applyViewportTransform(): void {
    this.#viewport.setAttribute(
      'transform',
      `translate(${this.#panX},${this.#panY}) scale(${this.#scale})`,
    );
  }

  #onBgPointerDown(e: PointerEvent): void {
    if ((e.target as Element).closest('.node-group') || (e.target as Element).closest('.edge-hit'))
      return;

    const startX = e.clientX;
    const startY = e.clientY;
    const originX = this.#panX;
    const originY = this.#panY;
    this.#svg.setPointerCapture(e.pointerId);

    const onMove = (ev: PointerEvent) => {
      this.#panX = originX + (ev.clientX - startX);
      this.#panY = originY + (ev.clientY - startY);
      this.#applyViewportTransform();
    };
    const onUp = (ev: PointerEvent) => {
      this.#svg.releasePointerCapture(ev.pointerId);
      this.#svg.removeEventListener('pointermove', onMove as EventListener);
      this.#svg.removeEventListener('pointerup', onUp as EventListener);
      const dist = Math.abs(ev.clientX - startX) + Math.abs(ev.clientY - startY);
      if (dist < 4) {
        this.#controller?.select(null);
        this.#controller?.selectEdge(null);
        const rect = this.#svg.getBoundingClientRect();
        this.#lastClickX = (ev.clientX - rect.left - this.#panX) / this.#scale;
        this.#lastClickY = (ev.clientY - rect.top - this.#panY) / this.#scale;
      }
    };

    this.#svg.addEventListener('pointermove', onMove as EventListener);
    this.#svg.addEventListener('pointerup', onUp as EventListener);
  }

  #onWheel(e: WheelEvent): void {
    e.preventDefault();
    const factor = e.deltaY > 0 ? 0.95 : 1.05;
    const newScale = Math.min(4, Math.max(0.1, this.#scale * factor));
    const rect = this.#svg.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    this.#panX = mx - ((mx - this.#panX) * newScale) / this.#scale;
    this.#panY = my - ((my - this.#panY) * newScale) / this.#scale;
    this.#scale = newScale;
    this.#applyViewportTransform();
  }

  #onDrop(e: DragEvent): void {
    const indexStr = e.dataTransfer?.getData('witgraph/component-index');
    if (!indexStr || !this.#controller) return;
    e.preventDefault();
    const rect = this.#svg.getBoundingClientRect();
    const x = (e.clientX - rect.left - this.#panX) / this.#scale;
    const y = (e.clientY - rect.top - this.#panY) / this.#scale;
    this.#controller.addNodeFromCatalog(parseInt(indexStr, 10), x, y);
  }

  #render(): void {
    if (!this.#controller || !this.#svg) return;
    const nodes = this.#controller.nodes;
    const edges = this.#controller.edges;
    const selectedNodeId = this.#controller.selectedNode;
    const selectedEdgeId = this.#controller.selectedEdge;

    // Diff nodes: a node whose size, ports or preview changed is drawn again.
    for (const id of [...this.#nodeEls.keys()]) {
      if (!nodes.has(id)) this.#removeNodeGroup(id);
    }
    for (const [id, node] of nodes) {
      let g = this.#nodeEls.get(id);
      if (g && id !== this.#resizing && g.dataset.layout !== layoutKey(node)) {
        // In the same place among its siblings, so it keeps its stacking.
        const after = g.nextSibling;
        this.#removeNodeGroup(id);
        g = this.#createNodeGroup(node);
        this.#nodesGroup.insertBefore(g, after);
        this.#nodeEls.set(id, g);
        this.#attachNodeDrag(id, g);
      } else if (!g) {
        g = this.#createNodeGroup(node);
        this.#nodesGroup.appendChild(g);
        this.#nodeEls.set(id, g);
        this.#attachNodeDrag(id, g);
      } else {
        this.#updateNodeGroup(g, node);
      }
      g.classList.toggle('selected', id === selectedNodeId);
      g.classList.toggle('paused-here', id === this.#controller?.pausedNode);
      this.#markDiagnostics(g, this.#controller?.diagnosticsForNode(id) ?? []);
    }
    this.#updatePreviews();

    // Diff edges
    const edgeIds = new Set(edges.map((e) => e.id));
    for (const [id, els] of this.#edgeEls) {
      if (!edgeIds.has(id)) {
        els.visible.remove();
        els.hit.remove();
        this.#edgeEls.delete(id);
      }
    }
    for (const edge of edges) {
      let entry = this.#edgeEls.get(edge.id);
      if (!entry) {
        const visible = document.createElementNS(SVG_NS, 'path');
        visible.classList.add('edge');
        visible.classList.add(`edge-${this.#edgePortKind(edge)}`);
        if (edge.feedback) visible.classList.add('edge-feedback');
        visible.setAttribute('data-edge-id', edge.id);

        const hit = document.createElementNS(SVG_NS, 'path');
        hit.classList.add('edge-hit');
        hit.setAttribute('data-edge-id', edge.id);
        hit.addEventListener('click', (e) => {
          e.stopPropagation();
          this.#controller?.selectEdge(edge.id);
        });

        this.#edgesGroup.appendChild(hit);
        this.#edgesGroup.appendChild(visible);
        entry = { visible, hit };
        this.#edgeEls.set(edge.id, entry);
      }
      const d = this.#computeEdgePath(edge);
      if (d) {
        entry.visible.setAttribute('d', d);
        entry.hit.setAttribute('d', d);
      }
      entry.visible.classList.toggle('selected', edge.id === selectedEdgeId);
      this.#markDiagnostics(
        entry.visible,
        this.#controller?.diagnosticsForConnection(edge.id) ?? [],
      );
    }
  }

  /** Marks what compiling had something to say of, and says it on hovering. */
  #markDiagnostics(element: SVGElement, said: readonly Diagnostic[]): void {
    const diagnostics = said.filter((d) => !this.#controller?.isSettled(d));
    const error = diagnostics.some(({ severity }) => severity === 'error');
    element.classList.toggle('has-error', error);
    element.classList.toggle('has-warning', !error && diagnostics.length > 0);
    let title = element.querySelector(':scope > title.diagnostics');
    if (diagnostics.length === 0) {
      title?.remove();
      return;
    }
    if (!title) {
      title = document.createElementNS(SVG_NS, 'title');
      title.classList.add('diagnostics');
      element.prepend(title);
    }
    title.textContent = diagnostics.map((d) => d.message).join('\n');
  }

  /** Removes a node's group, its drag handlers and its viewer. */
  #removeNodeGroup(id: string): void {
    this.#nodeEls.get(id)?.remove();
    this.#nodeEls.delete(id);
    for (const key of [id, `${id}:resize`]) {
      this.#dragCleanups.get(key)?.();
      this.#dragCleanups.delete(key);
    }
    this.#previews.get(id)?.viewer.dispose();
    this.#previews.delete(id);
  }

  /** Shows each previewing node's output as it is now. */
  #updatePreviews(): void {
    if (!this.#controller) return;
    for (const [id, { port, viewer }] of this.#previews) {
      viewer.update(this.#controller.observe(id, port));
    }
  }

  #createNodeGroup(node: NodeVisual): SVGGElement {
    const g = document.createElementNS(SVG_NS, 'g');
    g.classList.add('node-group');
    g.setAttribute('data-node-id', node.id);
    g.setAttribute('transform', `translate(${node.x},${node.y})`);
    g.dataset.layout = layoutKey(node);

    const body = document.createElementNS(SVG_NS, 'rect');
    body.classList.add('node-body');
    body.setAttribute('width', String(node.width));
    body.setAttribute('height', String(node.height));
    g.appendChild(body);

    const header = document.createElementNS(SVG_NS, 'rect');
    header.classList.add('node-header', node.phase);
    header.setAttribute('width', String(node.width));
    header.setAttribute('height', String(NODE_HEADER_HEIGHT));
    g.appendChild(header);

    const label = document.createElementNS(SVG_NS, 'text');
    label.classList.add('node-label');
    label.setAttribute('x', String(node.width / 2));
    label.setAttribute('y', String(NODE_HEADER_HEIGHT / 2));
    label.textContent = node.label;
    g.appendChild(label);

    this.#appendPorts(g, node, 'input');
    this.#appendPorts(g, node, 'output');
    this.#appendFields(g, node);
    if (node.breakpoint) g.appendChild(this.#makeBreakpoint(node.width));
    if (node.preview) this.#appendPreview(g, node, node.preview);
    this.#appendResizeHandle(g, node);

    g.addEventListener('click', (e) => {
      e.stopPropagation();
      if (this.#didDrag) {
        this.#didDrag = false;
        return;
      }
      this.#controller?.select(node.id);
    });
    return g;
  }

  /** The node's preview region, below its ports band, with its viewer mounted. */
  #appendPreview(g: SVGGElement, node: NodeVisual, preview: NodePreview): void {
    const fo = document.createElementNS(SVG_NS, 'foreignObject');
    fo.classList.add('node-preview');
    fo.setAttribute('x', '0');
    fo.setAttribute('y', String(portsBottom(node)));
    fo.setAttribute('width', String(node.width));
    fo.setAttribute('height', String(preview.height));
    fo.setAttribute('data-preview-port', preview.port);
    const host = document.createElement('div');
    host.className = 'node-preview-host';
    fo.appendChild(host);
    g.appendChild(fo);
    g.classList.add('has-preview');
    const port = node.outputs.find((p) => p.name === preview.port);
    if (!port) return;
    const viewer = viewerFor(preview.viewer).mount(host, port);
    this.#previews.get(node.id)?.viewer.dispose();
    this.#previews.set(node.id, { port: preview.port, viewer });
  }

  /**
   * A grip at the node's bottom-right corner: drag it to resize the node. What takes the pointer
   * is the corner; what shows is two short lines inside its rounding.
   */
  #appendResizeHandle(g: SVGGElement, node: NodeVisual): void {
    const size = 12;
    const handle = document.createElementNS(SVG_NS, 'path');
    handle.classList.add('node-resize');
    const grip = document.createElementNS(SVG_NS, 'path');
    grip.classList.add('node-resize-grip');
    const place = (width: number, height: number) => {
      handle.setAttribute(
        'd',
        `M ${width},${height - size} L ${width},${height} L ${width - size},${height} Z`,
      );
      const [x, y] = [width - 3, height - 3];
      grip.setAttribute('d', `M ${x},${y - 7} L ${x - 7},${y} M ${x},${y - 3} L ${x - 3},${y}`);
    };
    place(node.width, node.height);
    g.append(handle, grip);

    let width = node.width;
    let height = node.height;
    let moved = false;
    // Only a node with a preview grows taller than its ports need.
    const minHeight = portsBottom(node) + (node.preview ? MIN_PREVIEW_HEIGHT : 0);
    const cleanup = makeDraggable(handle, {
      onStart: () => {
        this.#resizing = node.id;
        handle.classList.add('active');
      },
      onMove: (dx, dy) => {
        moved = true;
        width = Math.max(MIN_NODE_WIDTH, node.width + dx / this.#scale);
        height = node.preview ? Math.max(minHeight, node.height + dy / this.#scale) : node.height;
        // Only the outline follows the pointer; the node is drawn again at
        // its new size when the drag ends.
        g.querySelector('.node-body')?.setAttribute('width', String(width));
        g.querySelector('.node-body')?.setAttribute('height', String(height));
        g.querySelector('.node-header')?.setAttribute('width', String(width));
        const fo = g.querySelector('.node-preview');
        fo?.setAttribute('width', String(width));
        for (const field of g.querySelectorAll('.node-field')) {
          field.setAttribute('width', String(fieldWidth(width)));
        }
        fo?.setAttribute('height', String(height - portsBottom(node)));
        place(width, height);
      },
      onEnd: () => {
        this.#resizing = null;
        handle.classList.remove('active');
        // A click on the grip is no resize.
        if (moved) this.#controller?.resizeNode(node.id, width, height);
        // Unchanged, the node is not drawn again: put the handle back.
        place(width, height);
      },
    });
    this.#dragCleanups.get(`${node.id}:resize`)?.();
    this.#dragCleanups.set(`${node.id}:resize`, cleanup);
  }

  #appendPorts(g: SVGGElement, node: NodeVisual, side: 'input' | 'output'): void {
    const ports = side === 'input' ? node.inputs : node.outputs;
    for (const port of ports) {
      const at = portOffset(node, side, port.name);
      if (!at) continue;
      const { x: localX, y: localY } = at;

      const circle = document.createElementNS(SVG_NS, 'circle');
      circle.classList.add('port', `port-${port.kind}`);
      circle.setAttribute('cx', String(localX));
      circle.setAttribute('cy', String(localY));
      circle.setAttribute('r', String(PORT_RADIUS));
      circle.setAttribute('data-port-node', node.id);
      circle.setAttribute('data-port-name', port.name);
      circle.setAttribute('data-port-side', side);
      const title = document.createElementNS(SVG_NS, 'title');
      title.textContent = `${port.name}: ${port.typeDisplay}`;
      circle.appendChild(title);

      circle.addEventListener('pointerdown', (e) => {
        e.stopPropagation();
        this.#startConnection(node.id, port.name, side, node.x + localX, node.y + localY, e);
      });
      g.appendChild(circle);

      // An input with a field is named in it.
      if (side === 'input' && hasField(port)) continue;
      const lbl = document.createElementNS(SVG_NS, 'text');
      lbl.classList.add('port-label');
      lbl.setAttribute('dominant-baseline', 'central');
      lbl.setAttribute('y', String(localY));
      if (side === 'input') {
        lbl.setAttribute('x', String(localX + PORT_RADIUS + 4));
        lbl.setAttribute('text-anchor', 'start');
      } else {
        lbl.setAttribute('x', String(localX - PORT_RADIUS - 4));
        lbl.setAttribute('text-anchor', 'end');
      }
      lbl.textContent = port.name;
      g.appendChild(lbl);
    }
  }

  /** A row for each Value input, beside its port: its name, then its value. */
  #appendFields(g: SVGGElement, node: NodeVisual): void {
    // The names take the width of the longest, so that the values line up.
    const longest = Math.max(0, ...node.inputs.filter(hasField).map((p) => p.name.length));
    for (const port of node.inputs) {
      if (!hasField(port)) continue;
      const at = portOffset(node, 'input', port.name);
      if (!at) continue;
      const fo = document.createElementNS(SVG_NS, 'foreignObject');
      fo.classList.add('node-field');
      fo.setAttribute('x', String(FIELD_X));
      fo.setAttribute('y', String(at.y - FIELD_ROW_HEIGHT / 2 + 2));
      fo.setAttribute('width', String(fieldWidth(node.width)));
      fo.setAttribute('height', String(FIELD_ROW_HEIGHT - 4));
      fo.setAttribute('data-field-port', port.name);
      const row = document.createElement('div');
      row.className = 'node-field-row';
      row.style.setProperty('--_name-width', `${longest}ch`);
      const name = document.createElement('span');
      name.className = 'node-field-name';
      name.textContent = port.name;
      name.title = `${port.name}: ${port.typeDisplay}${port.docs ? `\n${port.docs}` : ''}`;
      row.appendChild(name);
      fo.appendChild(row);
      // The rest of the node drags it: in a field, the pointer is the field's.
      fo.addEventListener('pointerdown', (e) => e.stopPropagation());
      fo.addEventListener('contextmenu', (e) => e.stopPropagation());
      g.appendChild(fo);
    }
    this.#updateFields(g, node);
  }

  /**
   * Fills each of a node's fields as its input is now: a box to type a value into when
   * the input takes one, or else the value its connection gives it.
   */
  #updateFields(g: SVGGElement, node: NodeVisual): void {
    const controller = this.#controller;
    if (!controller) return;
    for (const fo of g.querySelectorAll<SVGForeignObjectElement>('.node-field')) {
      const port = node.inputs.find((p) => p.name === fo.dataset.fieldPort);
      const row = fo.firstElementChild;
      if (!port || !row) continue;
      const slot = row.children[1] as HTMLElement | undefined;
      if (controller.isInjectable(node.id, port.name)) {
        let input = slot instanceof HTMLInputElement ? slot : undefined;
        if (!input) {
          input = this.#makeFieldInput(node.id, port.name, port.typeDisplay);
          slot?.remove();
          row.appendChild(input);
        }
        // Not while it is typed in, nor over what failed to inject.
        if (input.classList.contains('invalid')) continue;
        input.title = controller.isPortConnected(node.id, port.name)
          ? 'Its value on the first tick, before its feedback connection gives it one'
          : '';
        if (this.shadowRoot?.activeElement !== input) {
          input.value = controller.getInjectedValue(node.id, port.name) ?? '';
        }
      } else {
        let shown = slot?.classList.contains('node-field-value') ? slot : undefined;
        if (!shown) {
          shown = document.createElement('span');
          shown.className = 'node-field-value';
          slot?.remove();
          row.appendChild(shown);
        }
        const from = controller.connectionForInput(node.id, port.name);
        const value = from ? controller.latestOutput(from.fromNode, from.fromPort) : undefined;
        const before = this.#fieldShown.get(shown);
        // Its text is drawn again only when it changes: for a new value, or once the text of
        // one that was coming is there.
        const text = from ? outputText(controller, from.fromNode, from.fromPort) : '-';
        if (before && before.value === value && before.text === text) continue;
        this.#fieldShown.set(shown, { value, text });
        shown.textContent = text;
        shown.title = from ? `${text}\nfrom ${from.fromNode}.${from.fromPort}` : text;
      }
    }
  }

  /**
   * A box for an input's value as WAVE text: Enter, or leaving it, injects what it holds,
   * and an empty one clears the input. Escape puts back what is injected.
   */
  #makeFieldInput(node: string, port: string, type: string): HTMLInputElement {
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'node-field-input';
    input.placeholder = type;
    input.spellcheck = false;
    input.autocomplete = 'off';
    const commit = async () => {
      const controller = this.#controller;
      if (!controller) return;
      const text = input.value.trim();
      const injected = controller.getInjectedValue(node, port);
      if (text === (injected ?? '')) {
        input.classList.remove('invalid');
        return;
      }
      try {
        if (text === '') await controller.clearInput(node, port);
        else await controller.injectValue(node, port, text);
        input.classList.remove('invalid');
      } catch (error) {
        input.classList.add('invalid');
        input.title = error instanceof Error ? error.message : String(error);
      }
    };
    input.addEventListener('keydown', (e) => {
      // The debugger's F keys work from here too; the canvas's shortcuts would edit the graph.
      if (!/^F\d+$/.test(e.key)) e.stopPropagation();
      if (e.key === 'Enter') {
        e.preventDefault();
        void commit();
      } else if (e.key === 'Escape') {
        input.classList.remove('invalid');
        input.value = this.#controller?.getInjectedValue(node, port) ?? '';
        input.blur();
      }
    });
    input.addEventListener('blur', () => void commit());
    return input;
  }

  #makeBreakpoint(nodeWidth: number): SVGPolygonElement {
    const cx = nodeWidth - 12,
      cy = NODE_HEADER_HEIGHT / 2,
      s = 5;
    const poly = document.createElementNS(SVG_NS, 'polygon');
    poly.classList.add('breakpoint');
    poly.setAttribute('points', `${cx},${cy - s} ${cx + s},${cy} ${cx},${cy + s} ${cx - s},${cy}`);
    return poly;
  }

  #updateNodeGroup(g: SVGGElement, node: NodeVisual): void {
    g.setAttribute('transform', `translate(${node.x},${node.y})`);
    const header = g.querySelector('.node-header');
    if (header) {
      for (const p of PHASES) header.classList.remove(p);
      header.classList.add(node.phase);
    }
    const label = g.querySelector('.node-label');
    if (label) label.textContent = node.label;

    const bp = g.querySelector('.breakpoint');
    if (node.breakpoint && !bp) g.appendChild(this.#makeBreakpoint(node.width));
    else if (!node.breakpoint && bp) bp.remove();
    this.#updateFields(g, node);
  }

  #edgePortKind(edge: EdgeVisual): string {
    const src = this.#controller?.nodes.get(edge.fromNode);
    const port = src?.outputs.find((p) => p.name === edge.fromPort);
    return port?.kind ?? 'default';
  }

  #computeEdgePath(edge: EdgeVisual): string | null {
    const nodes = this.#controller?.nodes;
    if (!nodes) return null;
    const src = nodes.get(edge.fromNode);
    const tgt = nodes.get(edge.toNode);
    if (!src || !tgt) return null;
    const from = portOffset(src, 'output', edge.fromPort);
    const to = portOffset(tgt, 'input', edge.toPort);
    if (!from || !to) return null;
    return edgePath(src.x + from.x, src.y + from.y, tgt.x + to.x, tgt.y + to.y);
  }

  #attachNodeDrag(nodeId: string, group: SVGGElement): void {
    let origX = 0,
      origY = 0;
    const cleanup = makeDraggable(
      group,
      {
        onStart: () => {
          const node = this.#controller?.nodes.get(nodeId);
          if (node) {
            origX = node.x;
            origY = node.y;
          }
          this.#didDrag = false;
        },
        onMove: (dx, dy) => {
          this.#didDrag = true;
          const newX = origX + dx / this.#scale;
          const newY = origY + dy / this.#scale;
          this.#controller?.moveNode(nodeId, newX, newY);
          group.setAttribute('transform', `translate(${newX},${newY})`);
          this.#refreshEdgesFor(nodeId);
        },
        onEnd: () => {
          this.#refreshEdgesFor(nodeId);
          const node = this.#controller?.nodes.get(nodeId);
          if (node && this.#didDrag) {
            this.#controller?.commitMove(nodeId, origX, origY, node.x, node.y);
          }
        },
      },
      '.port, .node-resize',
    );
    this.#dragCleanups.set(nodeId, cleanup);
  }

  #refreshEdgesFor(nodeId: string): void {
    if (!this.#controller) return;
    for (const edge of this.#controller.edges) {
      if (edge.fromNode === nodeId || edge.toNode === nodeId) {
        const entry = this.#edgeEls.get(edge.id);
        const d = this.#computeEdgePath(edge);
        if (entry && d) {
          entry.visible.setAttribute('d', d);
          entry.hit.setAttribute('d', d);
        }
      }
    }
  }

  #onKeyDown(e: KeyboardEvent): void {
    if (!this.#controller) return;
    const mod = e.metaKey || e.ctrlKey;

    if (mod && e.key === 'z' && !e.shiftKey) {
      e.preventDefault();
      this.#controller.undo();
      return;
    }
    if (mod && (e.key === 'Z' || e.key === 'y')) {
      e.preventDefault();
      this.#controller.redo();
      return;
    }
    if (mod && e.key === 'c') {
      e.preventDefault();
      this.#controller.copySelected();
      return;
    }
    if (mod && e.key === 'x') {
      e.preventDefault();
      this.#controller.cutSelected();
      return;
    }
    if (mod && e.key === 'v') {
      e.preventDefault();
      this.#controller.paste(this.#lastClickX + 30, this.#lastClickY + 30);
      return;
    }
    if (e.key === 'Delete' || e.key === 'Backspace') {
      const selectedNode = this.#controller.selectedNode;
      const selectedEdge = this.#controller.selectedEdge;
      if (selectedNode) this.#controller.removeNode(selectedNode);
      else if (selectedEdge) this.#controller.removeConnection(selectedEdge);
    }
  }

  #onContextMenu(e: MouseEvent): void {
    e.preventDefault();
    if (!this.#controller || !this.shadowRoot) return;
    this.#contextMenu.hide();

    const target = e.target as Element;
    const nodeGroup = target.closest('.node-group');
    const edgeHit = target.closest('.edge-hit');
    const rect = this.#svg.getBoundingClientRect();
    const menuX = e.clientX - rect.left;
    const menuY = e.clientY - rect.top;
    const graphX = (e.clientX - rect.left - this.#panX) / this.#scale;
    const graphY = (e.clientY - rect.top - this.#panY) / this.#scale;

    let items: MenuItem[];

    if (nodeGroup) {
      const nodeId = nodeGroup.getAttribute('data-node-id') ?? '';
      this.#controller.select(nodeId);
      const node = this.#controller.nodes.get(nodeId);
      items = [
        { label: 'Copy', shortcut: 'Ctrl+C', action: () => this.#controller!.copySelected() },
        { label: 'Cut', shortcut: 'Ctrl+X', action: () => this.#controller!.cutSelected() },
        { label: 'Delete', shortcut: 'Del', action: () => this.#controller!.removeNode(nodeId) },
        { label: '', separator: true, action: () => {} },
        {
          label: node?.breakpoint ? 'Remove Breakpoint' : 'Toggle Breakpoint',
          action: () => this.#controller!.toggleBreakpoint(nodeId),
        },
      ];
    } else if (edgeHit) {
      const edgeId = edgeHit.getAttribute('data-edge-id') ?? '';
      this.#controller.selectEdge(edgeId);
      const edge = this.#controller.edges.find((e) => e.id === edgeId);
      items = [
        {
          label: 'Delete Connection',
          shortcut: 'Del',
          action: () => this.#controller!.removeConnection(edgeId),
        },
        {
          label: edge?.feedback ? 'Remove Feedback' : 'Mark as Feedback',
          action: () => this.#controller!.setConnectionFeedback(edgeId, !edge?.feedback),
        },
      ];
    } else {
      items = [
        {
          label: 'Paste',
          shortcut: 'Ctrl+V',
          disabled: !this.#controller.hasClipboard,
          action: () => this.#controller!.paste(graphX, graphY),
        },
        { label: '', separator: true, action: () => {} },
        {
          label: 'Auto Layout',
          action: () => {
            void this.#controller!.autoLayout();
          },
        },
        { label: '', separator: true, action: () => {} },
        {
          label: 'Undo',
          shortcut: 'Ctrl+Z',
          disabled: !this.#controller.canUndo,
          action: () => this.#controller!.undo(),
        },
        {
          label: 'Redo',
          shortcut: 'Ctrl+Y',
          disabled: !this.#controller.canRedo,
          action: () => this.#controller!.redo(),
        },
      ];
    }

    this.#contextMenu.show(menuX, menuY, items, this.shadowRoot);
  }

  #connectSide: 'input' | 'output' = 'output';

  #startConnection(
    nodeId: string,
    portName: string,
    side: 'input' | 'output',
    startX: number,
    startY: number,
    e: PointerEvent,
  ): void {
    this.#connectFrom = { node: nodeId, port: portName, x: startX, y: startY };
    this.#connectSide = side;

    const temp = document.createElementNS(SVG_NS, 'path');
    temp.classList.add('temp-edge');
    this.#viewport.appendChild(temp);

    const svgRect = this.#svg.getBoundingClientRect();
    const toGraph = (clientX: number, clientY: number) => ({
      x: (clientX - svgRect.left - this.#panX) / this.#scale,
      y: (clientY - svgRect.top - this.#panY) / this.#scale,
    });

    const onMove = (ev: PointerEvent) => {
      const pos = toGraph(ev.clientX, ev.clientY);
      if (side === 'output') {
        temp.setAttribute('d', edgePath(startX, startY, pos.x, pos.y));
      } else {
        temp.setAttribute('d', edgePath(pos.x, pos.y, startX, startY));
      }
      // Shift held → dashed line indicating feedback
      temp.style.strokeDasharray = ev.shiftKey ? '6 3' : '';
      this.#highlightDropTarget(ev);
    };

    const onUp = (ev: PointerEvent) => {
      this.#svg.releasePointerCapture(ev.pointerId);
      this.#svg.removeEventListener('pointermove', onMove as EventListener);
      this.#svg.removeEventListener('pointerup', onUp as EventListener);
      temp.remove();
      this.#clearDropHighlights();

      const target = this.#findPortAt(ev);
      if (target && this.#connectFrom) {
        const from =
          side === 'output'
            ? { node: this.#connectFrom.node, port: this.#connectFrom.port }
            : { node: target.node, port: target.port };
        const to =
          side === 'output'
            ? { node: target.node, port: target.port }
            : { node: this.#connectFrom.node, port: this.#connectFrom.port };
        const id = `c-${from.node}-${from.port}-${to.node}-${to.port}`;
        const shiftHeld = ev.shiftKey;
        const wouldCycle = this.#wouldCreateCycle(from.node, to.node);
        const feedback = shiftHeld || wouldCycle;
        this.#controller?.addConnection(id, from, to, feedback);
      }
      this.#connectFrom = null;
    };

    this.#svg.setPointerCapture(e.pointerId);
    this.#svg.addEventListener('pointermove', onMove as EventListener);
    this.#svg.addEventListener('pointerup', onUp as EventListener);
  }

  #wouldCreateCycle(fromNode: string, toNode: string): boolean {
    if (fromNode === toNode) return true;
    if (!this.#controller) return false;
    const edges = this.#controller.edges.filter((e) => !e.feedback);
    const visited = new Set<string>();
    const stack = [toNode];
    while (stack.length > 0) {
      const node = stack.pop()!;
      if (node === fromNode) return true;
      if (visited.has(node)) continue;
      visited.add(node);
      for (const e of edges) {
        if (e.fromNode === node) stack.push(e.toNode);
      }
    }
    return false;
  }

  #findPortAt(ev: PointerEvent): { node: string; port: string } | null {
    const el = this.shadowRoot?.elementFromPoint(ev.clientX, ev.clientY);
    if (!el?.classList.contains('port')) return null;
    const targetSide = el.getAttribute('data-port-side');
    if (targetSide === this.#connectSide) return null;
    const node = el.getAttribute('data-port-node');
    const port = el.getAttribute('data-port-name');
    if (!node || !port) return null;
    if (node === this.#connectFrom?.node) return null;
    return { node, port };
  }

  #highlightDropTarget(ev: PointerEvent): void {
    this.#clearDropHighlights();
    const el = this.shadowRoot?.elementFromPoint(ev.clientX, ev.clientY);
    if (el?.classList.contains('port') && el.getAttribute('data-port-side') !== this.#connectSide) {
      el.classList.add('port-drop-target');
    }
  }

  #clearDropHighlights(): void {
    for (const el of this.shadowRoot?.querySelectorAll('.port-drop-target') ?? []) {
      el.classList.remove('port-drop-target');
    }
  }
}

if (!customElements.get('witgraph-canvas'))
  customElements.define('witgraph-canvas', WitgraphCanvas);
