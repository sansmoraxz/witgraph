import { adoptStyles } from './shared-styles.ts';
import type { EditorController } from '../controller/editor-controller.ts';
import type { NodeVisual, PortVisual } from '../controller/types.ts';

const PHASE_BG: Record<string, string> = {
  pending: 'bg-wg-phase-pending', running: 'bg-wg-phase-running',
  idle: 'bg-wg-phase-idle', faulted: 'bg-wg-phase-faulted', cancelled: 'bg-wg-phase-cancelled',
};

const KIND_BG: Record<string, string> = {
  value: 'bg-wg-port-value', stream: 'bg-wg-port-stream', future: 'bg-wg-port-future',
};

const KIND_BORDER: Record<string, string> = {
  value: 'border-wg-port-value', stream: 'border-wg-port-stream', future: 'border-wg-port-future',
};

const BTN = 'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 cursor-pointer text-xs hover:bg-wg-surface-hover transition-all shadow-sm';

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow-y: auto; overflow-x: hidden; background-color: #1e1e2e; }
  :host::-webkit-scrollbar { width: 6px; }
  :host::-webkit-scrollbar-track { background: #1e1e2e; }
  :host::-webkit-scrollbar-thumb { background: #45475a; border-radius: 3px; }
  :host::-webkit-scrollbar-thumb:hover { background: #585b70; }
  select, input[type="text"], input[type="checkbox"], textarea {
    background-color: #1e1e2e; color: #cdd6f4;
    border: 1px solid #45475a; border-radius: 4px;
    padding: 4px 6px; font-size: 11px;
    font-family: system-ui, sans-serif;
  }
  select { cursor: pointer; appearance: none;
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='%23a6adc8' stroke-width='2'%3E%3Cpath d='M6 9l6 6 6-6'/%3E%3C/svg%3E");
    background-repeat: no-repeat; background-position: right 6px center; padding-right: 22px;
  }
  select:focus, input:focus, textarea:focus { border-color: #89b4fa; outline: none; }
  input[type="checkbox"] { accent-color: #89b4fa; width: 14px; height: 14px; cursor: pointer; padding: 0; }
  .fault-bg { background: rgba(243, 139, 168, 0.08); border-left: 2px solid #f38ba8; }
  .bp-bg { background: rgba(250, 179, 135, 0.08); border-left: 2px solid #fab387; }
  .port-val { white-space: pre-wrap; word-break: break-all; }
  .focus-ring:focus { border-color: #89b4fa; outline: none; box-shadow: 0 0 0 2px rgba(137,180,250,0.25); }
  .section-accent { border-left: 2px solid #89b4fa; padding-left: 8px; }
  .section-accent-in { border-left-color: #89b4fa; }
  .section-accent-out { border-left-color: #a6e3a1; }
  .port-card { transition: background-color 0.1s; }
  .port-card:hover { background-color: rgba(49,50,68,0.8); }
  .arrow { color: #6c7086; font-size: 1.5rem; line-height: 1; }
  .toggle-track { position: relative; width: 36px; height: 20px; border-radius: 10px;
    background: #45475a; cursor: pointer; transition: background 0.2s; }
  .toggle-track.on { background: #89b4fa; }
  .toggle-knob { position: absolute; top: 2px; left: 2px; width: 16px; height: 16px;
    border-radius: 50%; background: #cdd6f4; transition: left 0.2s; }
  .toggle-track.on .toggle-knob { left: 18px; }
`;

export class WitgraphInspector extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.render();
  #onOutputChanged = () => this.render();

  constructor() {
    super();
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);
  }

  set controller(ctrl: EditorController) {
    if (this.#controller) {
      this.#controller.removeEventListener('state-changed', this.#onStateChanged);
      this.#controller.removeEventListener('output-changed', this.#onOutputChanged);
    }
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    ctrl.addEventListener('output-changed', this.#onOutputChanged);
    this.render();
  }

  connectedCallback(): void { this.render(); }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller?.removeEventListener('output-changed', this.#onOutputChanged);
  }

  render(): void {
    const root = this.shadowRoot!;
    const old = root.querySelector('.inspector-root');
    if (old) old.remove();

    if (!this.#controller) { root.appendChild(this.#buildGraphProperties()); return; }

    const edgeId = this.#controller.selectedEdge;
    if (edgeId) {
      const edge = this.#controller.edges.find((e) => e.id === edgeId);
      if (edge) { root.appendChild(this.#buildEdgeInspector(edge)); return; }
    }

    const nodeId = this.#controller.selectedNode;
    if (!nodeId) { root.appendChild(this.#buildGraphProperties()); return; }
    const node = this.#controller.nodes.get(nodeId);
    if (!node) { root.appendChild(this.#buildGraphProperties()); return; }
    root.appendChild(this.#buildInspector(node));
  }

  #el(tag: string, cls: string, text?: string): HTMLElement {
    const el = document.createElement(tag);
    el.className = cls;
    if (text !== undefined) el.textContent = text;
    return el;
  }

  #buildGraphProperties(): HTMLDivElement {
    const wrap = this.#el('div', 'inspector-root p-3') as HTMLDivElement;
    const graph = this.#controller?.graphJson as Record<string, unknown> | undefined;
    const meta = (graph?.metadata ?? {}) as { name?: string; description?: string; attrs?: Record<string, string> };

    const titleBar = this.#el('div', 'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none mb-3');
    titleBar.appendChild(this.#el('span', 'text-lg', '\u{1F4CA}'));
    titleBar.appendChild(this.#el('span', '', 'Graph Properties'));
    wrap.appendChild(titleBar);

    // Name
    const nameRow = this.#el('div', 'mb-2');
    nameRow.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Name'));
    const nameInput = document.createElement('input');
    nameInput.type = 'text';
    nameInput.className = 'label-input w-full bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 text-sm';
    nameInput.value = meta.name ?? '';
    nameInput.addEventListener('change', () => this.#controller?.setGraphName(nameInput.value));
    nameRow.appendChild(nameInput);
    wrap.appendChild(nameRow);

    // Description
    const descRow = this.#el('div', 'mb-2');
    descRow.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Description'));
    const descInput = document.createElement('textarea');
    descInput.className = 'label-input w-full bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 text-sm';
    descInput.rows = 3;
    descInput.value = meta.description ?? '';
    descInput.addEventListener('change', () => this.#controller?.setGraphDescription(descInput.value));
    descRow.appendChild(descInput);
    wrap.appendChild(descRow);

    // Stats
    const stats = this.#el('div', 'mb-3 p-2 bg-wg-surface rounded-md');
    stats.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Statistics'));
    stats.appendChild(this.#el('div', 'text-sm', `${this.#controller?.nodes.size ?? 0} nodes`));
    stats.appendChild(this.#el('div', 'text-sm', `${this.#controller?.edges.length ?? 0} edges`));
    const islands = this.#controller?.islands ?? [];
    stats.appendChild(this.#el('div', 'text-sm', `${islands.length} execution island${islands.length !== 1 ? 's' : ''}`));
    if (islands.length > 0) {
      const islandList = this.#el('div', 'mt-1');
      islands.forEach((members, i) => {
        const row = this.#el('div', 'font-mono text-xs text-wg-text-dim py-0.5');
        const hasStream = members.length > 1;
        const badge = hasStream ? '🔗' : '·';
        row.textContent = `  ${badge} #${i}: ${members.join(', ')}`;
        islandList.appendChild(row);
      });
      stats.appendChild(islandList);
      stats.appendChild(this.#el('div', 'text-xs text-wg-text-dim mt-1',
        'Islands group nodes sharing stream/future edges. Value-only nodes run independently.'));
    }
    const compiles = this.#controller?.compiles;
    stats.appendChild(this.#el('div', `text-sm mt-1 ${compiles ? 'text-wg-success' : 'text-wg-error'}`,
      compiles ? 'Compiles' : `Error: ${this.#controller?.compileError ?? 'unknown'}`));
    wrap.appendChild(stats);

    // Custom attrs
    wrap.appendChild(this.#buildAttrsEditor('Graph Attributes', meta.attrs ?? {},
      (k, v) => this.#controller?.setGraphAttr(k, v),
      (k) => this.#controller?.removeGraphAttr(k),
    ));

    // Hint
    wrap.appendChild(this.#el('div', 'text-xs text-wg-text-dim mt-3 text-center', 'Select a node or edge to inspect its details'));

    return wrap;
  }

  #buildAttrsEditor(
    title: string,
    attrs: Record<string, string>,
    onSet: (key: string, value: string) => void,
    onRemove: (key: string) => void,
  ): HTMLElement {
    const section = this.#el('div', 'mb-3');
    section.appendChild(this.#el('div', 'font-semibold mb-1.5 text-wg-text border-b border-wg-surface pb-1', title));

    const editorAttrs = Object.entries(attrs).filter(([k]) => !k.startsWith('editor.') && !k.startsWith('inject.'));
    for (const [key, val] of editorAttrs) {
      const row = document.createElement('div');
      row.style.cssText = 'display:grid; grid-template-columns:1fr 1fr 20px; gap:4px; margin-bottom:4px; align-items:center;';
      const keySpan = this.#el('span', 'text-xs font-mono text-wg-text-dim truncate', key);
      row.appendChild(keySpan);
      const valInput = document.createElement('input');
      valInput.type = 'text';
      valInput.className = 'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
      valInput.style.minWidth = '0';
      valInput.value = val;
      valInput.addEventListener('change', () => onSet(key, valInput.value));
      row.appendChild(valInput);
      const delBtn = this.#el('button', 'text-wg-error cursor-pointer text-xs', '×');
      delBtn.addEventListener('click', () => onRemove(key));
      row.appendChild(delBtn);
      section.appendChild(row);
    }

    // Add new attr
    const addRow = document.createElement('div');
    addRow.style.cssText = 'display:grid; grid-template-columns:1fr 1fr 20px; gap:4px; margin-top:4px; align-items:center;';
    const keyInput = document.createElement('input');
    keyInput.type = 'text';
    keyInput.placeholder = 'key';
    keyInput.className = 'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    keyInput.style.minWidth = '0';
    const valInput = document.createElement('input');
    valInput.type = 'text';
    valInput.placeholder = 'value';
    valInput.className = 'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    valInput.style.minWidth = '0';
    const addBtn = this.#el('button', 'bg-wg-surface text-wg-text border border-wg-text-dim rounded cursor-pointer text-xs', '+');
    addBtn.addEventListener('click', () => {
      if (keyInput.value) { onSet(keyInput.value, valInput.value); keyInput.value = ''; valInput.value = ''; }
    });
    addRow.append(keyInput, valInput, addBtn);
    section.appendChild(addRow);

    return section;
  }

  #buildInspector(node: NodeVisual): HTMLDivElement {
    const wrap = this.#el('div', 'inspector-root p-3 flex flex-col gap-3') as HTMLDivElement;

    // Header
    const header = this.#el('div', 'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none');
    header.appendChild(this.#el('span', 'text-lg', '\u{1F4E6}'));
    header.appendChild(this.#el('span', 'flex-1', 'Node'));
    header.appendChild(this.#el('span',
      `px-2 py-1 rounded-md text-xs font-bold text-wg-bg shadow-sm ${PHASE_BG[node.phase] ?? ''}`,
      node.phase.toUpperCase()));
    wrap.appendChild(header);

    // Editable node ID
    const idRow = this.#el('div', 'flex items-center gap-2');
    idRow.appendChild(this.#el('span', 'text-xs text-wg-text-dim font-semibold select-none', 'ID'));
    const idInput = document.createElement('input');
    idInput.type = 'text';
    idInput.value = node.id;
    idInput.className = 'focus-ring flex-1 bg-wg-surface text-wg-text border border-wg-text-dim rounded-md px-2 py-1 font-mono text-sm';
    idInput.style.minWidth = '0';
    idInput.addEventListener('change', () => {
      const newId = idInput.value.trim();
      if (newId && newId !== node.id) this.#controller?.renameNode(node.id, newId);
    });
    idRow.appendChild(idInput);
    wrap.appendChild(idRow);

    // Component info
    const graphNode = ((this.#controller?.graphJson as Record<string, unknown>)?.nodes as Array<{ id: string; component: Record<string, unknown>; attrs?: Record<string, string> }>)
      ?.find((n) => n.id === node.id);
    if (graphNode?.component) {
      const comp = graphNode.component as { package?: { namespace?: string; name?: string; version?: string }; world?: string };
      const compInfo = this.#el('div', 'p-2 bg-wg-surface rounded-md text-xs');
      compInfo.appendChild(this.#el('div', 'text-wg-text-dim mb-1', 'Component'));
      const compId = [
        comp.package?.namespace, ':', comp.package?.name,
        comp.world ? `/${comp.world}` : '',
        comp.package?.version ? `@${comp.package.version}` : '',
      ].filter(Boolean).join('');
      compInfo.appendChild(this.#el('div', 'font-mono text-wg-primary truncate', compId));
      wrap.appendChild(compInfo);
    }

    // Label editor
    const labelRow = this.#el('div', 'flex items-center gap-2');
    labelRow.appendChild(this.#el('span', 'text-xs text-wg-text-dim font-semibold select-none', 'Label'));
    const labelInput = document.createElement('input');
    labelInput.type = 'text';
    labelInput.className = 'focus-ring flex-1 bg-wg-surface text-wg-text border border-wg-text-dim rounded-md px-2 py-1 text-sm';
    labelInput.value = node.label;
    labelInput.placeholder = 'Node label...';
    labelInput.addEventListener('change', () => this.#controller?.setNodeLabel(node.id, labelInput.value));
    labelRow.appendChild(labelInput);
    wrap.appendChild(labelRow);

    // Breakpoint inputs
    const bp = this.#controller?.breakpointState;
    if (bp && bp.node === node.id) {
      const section = this.#el('div', 'bp-bg rounded-md p-3');
      section.appendChild(this.#el('div', 'font-semibold text-wg-warning mb-2 text-sm', '⏸ Paused - Inputs'));
      for (const [key, val] of Object.entries(bp.inputs)) {
        const entry = this.#el('div', 'flex items-center gap-2 font-mono text-xs py-0.5');
        entry.appendChild(this.#el('span', 'text-wg-text font-semibold', key));
        entry.appendChild(this.#el('span', 'text-wg-primary bg-wg-primary-5 px-1.5 py-0.5 rounded', JSON.stringify(val)));
        section.appendChild(entry);
      }
      wrap.appendChild(section);
    }

    // Fault
    if (node.fault) {
      const fault = this.#el('div', 'fault-bg rounded-md p-3');
      fault.appendChild(this.#el('div', 'font-semibold text-wg-error text-sm mb-1', `⚠ ${node.fault.kind}`));
      fault.appendChild(this.#el('div', 'font-mono text-xs whitespace-pre-wrap text-wg-text', node.fault.message));
      if (node.culprit) fault.appendChild(this.#el('div', 'text-wg-text-dim text-xs mt-1', `Culprit: ${node.culprit}`));
      wrap.appendChild(fault);
    }

    // Ports
    if (node.inputs.length > 0)
      wrap.appendChild(this.#buildPortSection('Inputs', node.inputs, node, 'input'));
    if (node.outputs.length > 0)
      wrap.appendChild(this.#buildPortSection('Outputs', node.outputs, node, 'output'));

    // Custom attrs
    const gNode = ((this.#controller?.graphJson as Record<string, unknown>)?.nodes as Array<{ id: string; attrs?: Record<string, string> }>)
      ?.find((n) => n.id === node.id);
    if (gNode) {
      wrap.appendChild(this.#buildAttrsEditor('Node Attributes', gNode.attrs ?? {},
        (k, v) => this.#controller?.setNodeAttr(node.id, k, v),
        (k) => this.#controller?.removeNodeAttr(node.id, k),
      ));
    }

    return wrap;
  }

  #buildPortSection(title: string, ports: PortVisual[], node: NodeVisual, direction: 'input' | 'output'): HTMLElement {
    const section = this.#el('div', '');
    const header = this.#el('div', `section-accent ${direction === 'input' ? 'section-accent-in' : 'section-accent-out'} font-semibold text-sm mb-2`);
    header.textContent = `${title} (${ports.length})`;
    section.appendChild(header);

    for (const port of ports) {
      const card = this.#el('div', 'port-card bg-wg-surface-50 rounded-md p-2 mb-1.5 shadow-sm');
      const topRow = this.#el('div', 'flex items-center gap-1.5');
      topRow.appendChild(this.#el('span',
        `inline-block px-1.5 py-0.5 rounded text-2xs font-bold text-wg-bg uppercase ${KIND_BG[port.kind] ?? ''}`,
        port.kind));
      topRow.appendChild(this.#el('span', 'font-semibold text-sm', port.name));
      topRow.appendChild(this.#el('span', 'text-wg-text-dim font-mono text-2xs ml-1 truncate', port.typeDisplay));
      card.appendChild(topRow);

      if (direction === 'output') {
        const pre = this.#el('pre', 'port-val font-mono text-xs mt-1.5 p-2 bg-wg-bg rounded-md text-wg-primary');
        try { pre.textContent = this.#controller!.readOutputWave(node.id, port.name) ?? '-'; }
        catch { pre.textContent = '-'; }
        card.appendChild(pre);

      }

      if (direction === 'input') {
        card.appendChild(this.#buildConnectionSelector(node, port));
        const connected = this.#controller?.isPortConnected(node.id, port.name) ?? false;
        if (!connected && port.kind === 'value') {
          card.appendChild(this.#buildInjectForm(node, port));
        }
      }

      if (direction === 'output') {
        const conns = this.#controller?.connectionsFromOutput(node.id, port.name) ?? [];
        if (conns.length > 0) {
          const list = this.#el('div', 'mt-1.5 text-xs text-wg-text-dim');
          for (const c of conns) {
            const row = this.#el('div', 'flex items-center gap-1 py-0.5');
            row.appendChild(this.#el('span', '', `→ ${c.toNode}.${c.toPort}`));
            if (c.feedback) row.appendChild(this.#el('span', 'text-wg-warning text-2xs', '(feedback)'));
            list.appendChild(row);
          }
          card.appendChild(list);
        }
      }

      section.appendChild(card);
    }
    return section;
  }

  #buildConnectionSelector(node: NodeVisual, port: PortVisual): HTMLElement {
    const wrap = this.#el('div', 'mt-1.5');
    const conn = this.#controller?.connectionForInput(node.id, port.name);
    const compatible = this.#controller?.compatibleOutputsForInput(node.id, port.name) ?? [];

    const row = this.#el('div', 'flex items-center gap-1.5');
    row.appendChild(this.#el('span', 'text-2xs text-wg-text-dim', 'Source:'));

    const select = document.createElement('select');
    select.className = 'flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    select.style.minWidth = '0';

    const noneOpt = document.createElement('option');
    noneOpt.value = '';
    noneOpt.textContent = '- none -';
    select.appendChild(noneOpt);

    for (const out of compatible) {
      const opt = document.createElement('option');
      opt.value = `${out.node}:${out.port}`;
      opt.textContent = out.self ? `${out.label} (self)` : out.label;
      if (conn && conn.fromNode === out.node && conn.fromPort === out.port) {
        opt.selected = true;
      }
      select.appendChild(opt);
    }

    row.appendChild(select);
    wrap.appendChild(row);

    // Feedback checkbox (only when the port kind supports it)
    let fbCheck: HTMLInputElement | null = null;
    const canFeedback = port.kind === 'value';
    if (canFeedback) {
      const fbRow = this.#el('div', 'flex items-center gap-1 mt-1');
      fbCheck = document.createElement('input');
      fbCheck.type = 'checkbox';
      fbCheck.checked = conn?.feedback ?? false;
      fbCheck.id = `fb-${node.id}-${port.name}`;
      if (conn) {
        fbCheck.addEventListener('change', () => this.#controller?.setConnectionFeedback(conn.id, fbCheck!.checked));
      }
      const fbLabel = document.createElement('label');
      fbLabel.htmlFor = fbCheck.id;
      fbLabel.className = 'text-2xs text-wg-text-dim cursor-pointer';
      fbLabel.textContent = 'Feedback (unit delay)';
      fbRow.append(fbCheck, fbLabel);
      wrap.appendChild(fbRow);
    } else if (conn) {
      wrap.appendChild(this.#el('div', 'text-2xs text-wg-text-dim mt-1 italic',
        `${port.kind} ports cannot be feedback`));
    }

    select.addEventListener('change', () => {
      const val = select.value;
      if (!val) {
        this.#controller?.disconnectInput(node.id, port.name);
      } else {
        const [fromNode, fromPort] = val.split(':');
        const isSelf = fromNode === node.id;
        const feedback = fbCheck?.checked ?? isSelf;
        this.#controller?.connectInput(node.id, port.name, fromNode, fromPort, feedback);
      }
    });

    return wrap;
  }

  #buildInjectForm(node: NodeVisual, port: PortVisual): HTMLElement {
    const currentValue = this.#controller?.getInjectedValue(node.id, port.name);
    const wrap = this.#el('div', 'mt-1.5');

    if (currentValue !== undefined) {
      const valRow = this.#el('div', 'flex items-center gap-1.5 mb-1');
      valRow.appendChild(this.#el('span', 'text-2xs text-wg-text-dim', 'Injected:'));
      valRow.appendChild(this.#el('span', 'font-mono text-xs text-wg-success', currentValue));
      wrap.appendChild(valRow);
    }

    const form = this.#el('div', 'flex gap-1.5 items-center');
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'focus-ring flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-2 py-1 font-mono text-xs';
    input.style.minWidth = '0';
    input.placeholder = 'WAVE value…';
    if (currentValue !== undefined) input.value = currentValue;

    const injectBtn = this.#el('button', BTN, 'Inject');
    const clearBtn = this.#el('button', `${BTN} opacity-85`, '×');
    (clearBtn as HTMLElement).title = 'Clear';
    form.append(input, injectBtn, clearBtn);

    injectBtn.addEventListener('click', () => {
      form.parentElement?.querySelector('.inject-error')?.remove();
      try {
        this.#controller!.injectValue(node.id, port.name, input.value);
      } catch (err: unknown) {
        const errEl = this.#el('div', 'inject-error text-wg-error text-xs mt-1 p-1 bg-wg-error-10 rounded', err instanceof Error ? err.message : String(err));
        form.parentElement!.appendChild(errEl);
      }
    });

    clearBtn.addEventListener('click', () => {
      try { this.#controller!.clearInput(node.id, port.name); } catch {}
    });

    wrap.appendChild(form);
    return wrap;
  }

  #buildEdgeInspector(edge: { id: string; fromNode: string; fromPort: string; toNode: string; toPort: string; feedback: boolean }): HTMLDivElement {
    const wrap = this.#el('div', 'inspector-root p-3 flex flex-col gap-3') as HTMLDivElement;

    const edgeHeader = this.#el('div', 'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none');
    edgeHeader.appendChild(this.#el('span', 'text-lg', '\u{1F517}'));
    edgeHeader.appendChild(this.#el('span', '', 'Edge'));
    wrap.appendChild(edgeHeader);
    wrap.appendChild(this.#el('div', 'font-mono text-xs text-wg-text bg-wg-surface p-2 rounded-md shadow-sm truncate', edge.id));

    // From → To with arrow
    const flow = this.#el('div', 'flex items-center gap-3 p-3 bg-wg-surface rounded-md shadow-sm');
    const fromBox = this.#el('div', 'flex-1 text-center');
    fromBox.appendChild(this.#el('div', 'text-2xs text-wg-text-dim uppercase font-semibold mb-0.5', 'From'));
    fromBox.appendChild(this.#el('div', 'font-bold text-sm', edge.fromNode));
    fromBox.appendChild(this.#el('div', 'font-mono text-xs text-wg-primary', edge.fromPort));
    flow.appendChild(fromBox);
    flow.appendChild(this.#el('div', 'arrow select-none', '→'));
    const toBox = this.#el('div', 'flex-1 text-center');
    toBox.appendChild(this.#el('div', 'text-2xs text-wg-text-dim uppercase font-semibold mb-0.5', 'To'));
    toBox.appendChild(this.#el('div', 'font-bold text-sm', edge.toNode));
    toBox.appendChild(this.#el('div', 'font-mono text-xs text-wg-primary', edge.toPort));
    flow.appendChild(toBox);
    wrap.appendChild(flow);

    // Feedback toggle (Value ports only)
    const srcNode = this.#controller?.nodes.get(edge.fromNode);
    const srcPort = srcNode?.outputs.find((p) => p.name === edge.fromPort);
    const isValue = srcPort?.kind === 'value';

    if (isValue) {
      const fbRow = this.#el('div', 'flex items-center gap-3 p-2 bg-wg-surface rounded-md');
      const track = this.#el('div', `toggle-track ${edge.feedback ? 'on' : ''}`);
      track.appendChild(this.#el('div', 'toggle-knob'));
      track.addEventListener('click', () => {
        this.#controller?.setConnectionFeedback(edge.id, !edge.feedback);
      });
      fbRow.appendChild(track);
      const fbText = this.#el('div', '');
      fbText.appendChild(this.#el('div', 'text-sm font-semibold text-wg-text', 'Feedback'));
      fbText.appendChild(this.#el('div', 'text-2xs text-wg-text-dim', 'Unit delay - delivers previous tick value'));
      fbRow.appendChild(fbText);
      wrap.appendChild(fbRow);
    } else {
      wrap.appendChild(this.#el('div', 'text-xs text-wg-text-dim italic p-2 bg-wg-surface rounded-md',
        `${srcPort?.kind ?? 'unknown'} ports cannot be feedback`));
    }

    // Current value
    wrap.appendChild(this.#el('div', 'section-accent font-semibold text-sm', 'Current Value'));
    const valEl = this.#el('pre', 'port-val font-mono text-xs p-2 bg-wg-surface rounded-md text-wg-primary shadow-sm');
    try { valEl.textContent = this.#controller!.readOutputWave(edge.fromNode, edge.fromPort) ?? '-'; }
    catch { valEl.textContent = '-'; }
    wrap.appendChild(valEl);

    return wrap;
  }
}

customElements.define('witgraph-inspector', WitgraphInspector);
