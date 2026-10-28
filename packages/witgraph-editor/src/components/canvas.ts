import { NODE_WIDTH, PORT_RADIUS, NODE_HEADER_HEIGHT } from './styles.ts';
import { adoptStyles } from './shared-styles.ts';
import type { EditorController } from '../controller/editor-controller.ts';
import type { NodeVisual, EdgeVisual } from '../controller/types.ts';
import { edgePath, portPosition } from '../util/svg.ts';
import { makeDraggable } from '../util/drag.ts';
import { ContextMenu, type MenuItem } from './context-menu.ts';
import * as echarts from 'echarts';

const SVG_NS = 'http://www.w3.org/2000/svg';

const SVG_CSS = `
  :host { display: block; width: 100%; height: 100%; overflow: hidden; background-color: #1e1e2e; }
  svg { width: 100%; height: 100%; user-select: none; -webkit-user-select: none; }
  .node-group text { user-select: none; }
  .port-label, .node-label { pointer-events: none; }

  /* Node styling — solid with depth */
  .node-group { cursor: grab; filter: drop-shadow(0 2px 4px rgba(0,0,0,0.3)); transition: filter 0.15s; }
  .node-group:hover { filter: drop-shadow(0 3px 8px rgba(0,0,0,0.4)); }
  .node-group.selected { filter: drop-shadow(0 0 8px rgba(137,180,250,0.5)) drop-shadow(0 2px 4px rgba(0,0,0,0.3)); }
  .node-group.selected .node-body { stroke: #89b4fa; stroke-width: 2; }
  .node-body { fill: url(#node-gradient); stroke: #45475a; stroke-width: 1; rx: 8px; }
  .node-header { rx: 8px; }
  .node-header.pending { fill: #fab387; }
  .node-header.running { fill: #89b4fa; }
  .node-header.idle { fill: #a6e3a1; }
  .node-header.faulted { fill: #f38ba8; }
  .node-header.cancelled { fill: #f9e2af; }
  .node-label { fill: #1e1e2e; font-weight: 700; font-size: 11px; text-anchor: middle;
    dominant-baseline: central; pointer-events: none; letter-spacing: 0.02em;
    overflow: hidden; text-overflow: ellipsis; }

  /* Ports — larger with ring */
  .port { cursor: pointer; stroke: #1e1e2e; stroke-width: 2; transition: r 0.1s, stroke 0.1s; }
  .port-value { fill: #89b4fa; }
  .port-stream { fill: #cba6f7; }
  .port-future { fill: #f9e2af; }
  .port:hover { r: 8; stroke: #cdd6f4; stroke-width: 2; }
  .port-drop-target { r: 9; stroke: #a6e3a1; stroke-width: 2.5; }
  .port-label { fill: #cdd6f4; font-size: 10px; pointer-events: none; font-weight: 500; }

  /* Edges — colored by type */
  .edge { fill: none; stroke-width: 2.5; stroke-linecap: round; transition: stroke-width 0.1s, filter 0.15s; }
  .edge-value { stroke: #89b4fa; }
  .edge-stream { stroke: #cba6f7; }
  .edge-future { stroke: #f9e2af; }
  .edge-default { stroke: #6c7086; }
  .edge-feedback { stroke-dasharray: 8 4; }
  .edge.selected { stroke-width: 3.5; filter: drop-shadow(0 0 4px rgba(137,180,250,0.4)); }
  .edge-hit { fill: none; stroke: transparent; stroke-width: 14; pointer-events: stroke; cursor: pointer; }
  .edge-hit:hover + .edge { stroke-width: 3; filter: drop-shadow(0 0 3px rgba(137,180,250,0.3)); }

  /* Breakpoint — pin icon */
  .breakpoint { fill: #f38ba8; filter: drop-shadow(0 0 3px rgba(243,139,168,0.5)); }

  /* Temp edge for connection drawing */
  .temp-edge { fill: none; stroke: #89b4fa; stroke-width: 2; stroke-dasharray: 6 4;
    pointer-events: none; opacity: 0.8; }

  /* Expand button */
  .expand-btn { cursor: pointer; fill: #cdd6f4; font-size: 11px; pointer-events: all; opacity: 0.6; }
  .expand-btn:hover { opacity: 1; }

  /* Chart container */
  .chart-container { background: #313244; border-radius: 0 0 8px 8px; }
`;

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

  #panX = 0;
  #panY = 0;
  #scale = 1;
  #didDrag = false;

  #tempEdge: SVGPathElement | null = null;
  #connectFrom: { node: string; port: string; x: number; y: number } | null = null;
  #chartInstances = new Map<string, echarts.ECharts>();
  #contextMenu = new ContextMenu();
  #lastClickX = 0;
  #lastClickY = 0;

  #onStateChanged = () => this.#render();

  connectedCallback(): void {
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
    dot.setAttribute('fill', '#313244');
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
    stop1.setAttribute('stop-color', '#3b3c56');
    const stop2 = document.createElementNS(SVG_NS, 'stop');
    stop2.setAttribute('offset', '100%');
    stop2.setAttribute('stop-color', '#2a2b3d');
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

    if (this.#controller) {
      this.#controller.addEventListener('state-changed', this.#onStateChanged);
      this.#render();
    }
  }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    for (const cleanup of this.#dragCleanups.values()) cleanup();
    this.#dragCleanups.clear();
  }

  set controller(ctrl: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    if (this.shadowRoot) this.#render();
  }

  get controller(): EditorController | null { return this.#controller; }

  #applyViewportTransform(): void {
    this.#viewport.setAttribute(
      'transform',
      `translate(${this.#panX},${this.#panY}) scale(${this.#scale})`,
    );
  }

  #onBgPointerDown(e: PointerEvent): void {
    if ((e.target as Element).closest('.node-group') || (e.target as Element).closest('.edge-hit')) return;

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

    // Diff nodes
    const liveIds = new Set(nodes.keys());
    for (const [id, el] of this.#nodeEls) {
      if (!liveIds.has(id)) {
        el.remove();
        this.#nodeEls.delete(id);
        this.#dragCleanups.get(id)?.();
        this.#dragCleanups.delete(id);
      }
    }
    for (const [id, node] of nodes) {
      let g = this.#nodeEls.get(id);
      if (!g) {
        g = this.#createNodeGroup(node);
        this.#nodesGroup.appendChild(g);
        this.#nodeEls.set(id, g);
        this.#attachNodeDrag(id, g);
      } else {
        this.#updateNodeGroup(g, node);
      }
      g.classList.toggle('selected', id === selectedNodeId);
    }

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
    }

    this.#updateExpandedCharts();
  }

  #updateExpandedCharts(): void {
    if (!this.#controller) return;

    for (const [id, node] of this.#controller.nodes) {
      const g = this.#nodeEls.get(id);
      if (!g) continue;

      const existing = g.querySelector('.node-chart-fo');
      if (!node.expanded) {
        if (existing) {
          const key = `${id}:`;
          for (const [k, chart] of this.#chartInstances) {
            if (k.startsWith(key)) { chart.dispose(); this.#chartInstances.delete(k); }
          }
          existing.remove();
        }
        continue;
      }

      const valuePorts = node.outputs.filter((p) => p.kind === 'value');
      if (valuePorts.length === 0) continue;

      const chartH = 100;
      const totalH = valuePorts.length * chartH;
      const foY = node.height;

      let fo = existing as SVGForeignObjectElement | null;
      if (!fo) {
        fo = document.createElementNS(SVG_NS, 'foreignObject');
        fo.classList.add('node-chart-fo');
        g.appendChild(fo);
      }
      fo.setAttribute('x', '0');
      fo.setAttribute('y', String(foY));
      fo.setAttribute('width', String(node.width));
      fo.setAttribute('height', String(totalH));

      for (const port of valuePorts) {
        const chartKey = `${id}:${port.name}`;
        let chart = this.#chartInstances.get(chartKey);
        let container = fo.querySelector<HTMLDivElement>(`[data-chart-port="${port.name}"]`);

        if (!container) {
          container = document.createElement('div');
          container.className = 'chart-container';
          container.setAttribute('data-chart-port', port.name);
          container.style.width = `${node.width}px`;
          container.style.height = `${chartH}px`;
          fo.appendChild(container);
        }

        if (!chart) {
          chart = echarts.init(container, undefined, { renderer: 'svg', width: node.width, height: chartH });
          this.#chartInstances.set(chartKey, chart);
        }

        const history = this.#controller!.outputHistory(id, port.name);
        const ticks = history.map((h) => h.tick);
        const values = history.map((h) => { const n = parseFloat(h.wave); return isNaN(n) ? 0 : n; });

        chart.setOption({
          animation: false,
          grid: { top: 18, right: 6, bottom: 18, left: 30 },
          title: { text: port.name, textStyle: { fontSize: 9, color: '#a6adc8' }, left: 4, top: 0 },
          xAxis: { type: 'category', data: ticks, axisLabel: { fontSize: 8, color: '#6c7086' }, axisLine: { lineStyle: { color: '#45475a' } } },
          yAxis: { type: 'value', axisLabel: { fontSize: 8, color: '#6c7086' }, splitLine: { lineStyle: { color: '#313244' } }, axisLine: { lineStyle: { color: '#45475a' } } },
          series: [{ type: 'line', data: values, smooth: true, symbol: 'circle', symbolSize: 3, lineStyle: { color: '#89b4fa', width: 1.5 }, itemStyle: { color: '#89b4fa' }, areaStyle: { color: 'rgba(137,180,250,0.1)' } }],
        });
      }
    }
  }

  #createNodeGroup(node: NodeVisual): SVGGElement {
    const g = document.createElementNS(SVG_NS, 'g');
    g.classList.add('node-group');
    g.setAttribute('data-node-id', node.id);
    g.setAttribute('transform', `translate(${node.x},${node.y})`);

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
    if (node.breakpoint) g.appendChild(this.#makeBreakpoint(node.width));


    g.addEventListener('click', (e) => {
      e.stopPropagation();
      if (this.#didDrag) { this.#didDrag = false; return; }
      this.#controller?.select(node.id);
    });
    return g;
  }

  #appendPorts(g: SVGGElement, node: NodeVisual, side: 'input' | 'output'): void {
    const ports = side === 'input' ? node.inputs : node.outputs;
    for (let i = 0; i < ports.length; i++) {
      const port = ports[i];
      const localX = side === 'input' ? 0 : node.width;
      const localY = NODE_HEADER_HEIGHT + ((node.height - NODE_HEADER_HEIGHT) * (i + 1)) / (ports.length + 1);

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

  #makeBreakpoint(nodeWidth: number): SVGPolygonElement {
    const cx = nodeWidth - 12, cy = NODE_HEADER_HEIGHT / 2, s = 5;
    const poly = document.createElementNS(SVG_NS, 'polygon');
    poly.classList.add('breakpoint');
    poly.setAttribute('points',
      `${cx},${cy - s} ${cx + s},${cy} ${cx},${cy + s} ${cx - s},${cy}`);
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
    const body = g.querySelector('.node-body');
    if (body) body.setAttribute('height', String(node.height));

    // Rebuild ports if the count changed
    const existingPorts = g.querySelectorAll('.port');
    const expectedPorts = node.inputs.length + node.outputs.length;
    if (existingPorts.length !== expectedPorts) {
      g.querySelectorAll('.port').forEach((el) => el.remove());
      g.querySelectorAll('.port-label').forEach((el) => el.remove());
      this.#appendPorts(g, node, 'input');
      this.#appendPorts(g, node, 'output');
    }

    const bp = g.querySelector('.breakpoint');
    if (node.breakpoint && !bp) g.appendChild(this.#makeBreakpoint(node.width));
    else if (!node.breakpoint && bp) bp.remove();
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
    const fi = src.outputs.findIndex((p) => p.name === edge.fromPort);
    const ti = tgt.inputs.findIndex((p) => p.name === edge.toPort);
    if (fi < 0 || ti < 0) return null;
    const from = portPosition(src.x, src.y, src.width, src.height, fi, src.outputs.length, 'output');
    const to = portPosition(tgt.x, tgt.y, tgt.width, tgt.height, ti, tgt.inputs.length, 'input');
    return edgePath(from.x, from.y, to.x, to.y);
  }

  #attachNodeDrag(nodeId: string, group: SVGGElement): void {
    let origX = 0, origY = 0;
    const cleanup = makeDraggable(group, {
      onStart: () => {
        const node = this.#controller?.nodes.get(nodeId);
        if (node) { origX = node.x; origY = node.y; }
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
    }, '.port');
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
        { label: node?.breakpoint ? 'Remove Breakpoint' : 'Toggle Breakpoint', action: () => this.#controller!.toggleBreakpoint(nodeId) },
        { label: node?.expanded ? 'Collapse Chart' : 'Expand Chart', action: () => this.#controller!.toggleExpanded(nodeId) },
      ];
    } else if (edgeHit) {
      const edgeId = edgeHit.getAttribute('data-edge-id') ?? '';
      this.#controller.selectEdge(edgeId);
      const edge = this.#controller.edges.find((e) => e.id === edgeId);
      items = [
        { label: 'Delete Connection', shortcut: 'Del', action: () => this.#controller!.removeConnection(edgeId) },
        { label: edge?.feedback ? 'Remove Feedback' : 'Mark as Feedback', action: () => this.#controller!.setConnectionFeedback(edgeId, !edge?.feedback) },
      ];
    } else {
      items = [
        { label: 'Paste', shortcut: 'Ctrl+V', disabled: !this.#controller.hasClipboard, action: () => this.#controller!.paste(graphX, graphY) },
        { label: '', separator: true, action: () => {} },
        { label: 'Auto Layout', action: () => { void this.#controller!.autoLayout(); } },
        { label: '', separator: true, action: () => {} },
        { label: 'Undo', shortcut: 'Ctrl+Z', disabled: !this.#controller.canUndo, action: () => this.#controller!.undo() },
        { label: 'Redo', shortcut: 'Ctrl+Y', disabled: !this.#controller.canRedo, action: () => this.#controller!.redo() },
      ];
    }

    this.#contextMenu.show(menuX, menuY, items, this.shadowRoot);
  }

  #connectSide: 'input' | 'output' = 'output';

  #startConnection(nodeId: string, portName: string, side: 'input' | 'output', startX: number, startY: number, e: PointerEvent): void {
    this.#connectFrom = { node: nodeId, port: portName, x: startX, y: startY };
    this.#connectSide = side;

    const temp = document.createElementNS(SVG_NS, 'path');
    temp.classList.add('temp-edge');
    this.#viewport.appendChild(temp);
    this.#tempEdge = temp;

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
      this.#tempEdge = null;
      this.#clearDropHighlights();

      const target = this.#findPortAt(ev);
      if (target && this.#connectFrom) {
        const from = side === 'output'
          ? { node: this.#connectFrom.node, port: this.#connectFrom.port }
          : { node: target.node, port: target.port };
        const to = side === 'output'
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
    if (!el || !el.classList.contains('port')) return null;
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
    this.shadowRoot?.querySelectorAll('.port-drop-target').forEach(
      (el) => el.classList.remove('port-drop-target'),
    );
  }
}

customElements.define('witgraph-canvas', WitgraphCanvas);
