import { createElement, History, Trash2 } from 'lucide';
import type { Diagnostic } from 'witgraph-loader';
import type { EditorController } from '../controller/editor-controller.ts';
import type { NodeVisual, PortVisual } from '../controller/types.ts';
import { defaultViewer, describeHandle } from '../controller/visual.ts';
import { weight } from '../util/weight.ts';
import { summarize } from '../viewers/text.ts';
import { adoptStyles } from './shared-styles.ts';

const PHASE_BG: Record<string, string> = {
  pending: 'bg-wg-phase-pending',
  running: 'bg-wg-phase-running',
  idle: 'bg-wg-phase-idle',
  faulted: 'bg-wg-phase-faulted',
  cancelled: 'bg-wg-phase-cancelled',
};

const KIND_BG: Record<string, string> = {
  value: 'bg-wg-port-value',
  stream: 'bg-wg-port-stream',
  future: 'bg-wg-port-future',
  handle: 'bg-wg-port-handle',
};

const BTN =
  'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 cursor-pointer text-xs hover:bg-wg-surface-hover transition-all shadow-sm';

/** The biggest value the inspector writes out as WAVE text, in bytes. */
const MAX_WAVE_BYTES = 64 * 1024;

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow-y: auto; overflow-x: hidden; background-color: var(--wg-bg);
    --_input-radius: var(--wg-input-radius, 4px);
    --_toggle-radius: var(--wg-toggle-radius, 10px);
    --_btn-radius: var(--wg-btn-radius, 4px); }
  .icon-btn { display: inline-flex; align-items: center; justify-content: center; padding: 5px;
    background: transparent; color: var(--wg-text-dim); border: none; border-radius: var(--_btn-radius);
    cursor: pointer; transition: all 0.12s ease; }
  .icon-btn svg { width: 14px; height: 14px; stroke-width: 2; }
  .icon-btn:hover { background: var(--wg-surface-hover); color: var(--wg-text); }
  .icon-btn.danger:hover { background: color-mix(in srgb, var(--wg-error) 15%, transparent); color: var(--wg-error); }
  .locked .toggle-track[data-structural] { opacity: 0.4; pointer-events: none; }
  .locked :disabled { opacity: 0.5; cursor: not-allowed; }
  :host::-webkit-scrollbar { width: 6px; }
  :host::-webkit-scrollbar-track { background: var(--wg-bg); }
  :host::-webkit-scrollbar-thumb { background: var(--wg-surface-hover); border-radius: 3px; }
  :host::-webkit-scrollbar-thumb:hover { background: var(--wg-overlay); }
  select, input[type="text"], input[type="checkbox"], textarea {
    background-color: var(--wg-bg); color: var(--wg-text);
    border: 1px solid var(--wg-surface-hover); border-radius: var(--_input-radius);
    padding: 4px 6px; font-size: 11px;
    font-family: system-ui, sans-serif;
  }
  select { cursor: pointer; appearance: none;
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='%23a6adc8' stroke-width='2'%3E%3Cpath d='M6 9l6 6 6-6'/%3E%3C/svg%3E");
    background-repeat: no-repeat; background-position: right 6px center; padding-right: 22px;
  }
  select:focus, input:focus, textarea:focus { border-color: var(--wg-primary); outline: none; }
  input[type="checkbox"] { accent-color: var(--wg-primary); width: 14px; height: 14px; cursor: pointer; padding: 0; }
  .settled-bg { background: var(--wg-surface); border-left: 2px solid var(--wg-text-muted); }
  .warning-bg { background: color-mix(in srgb, var(--wg-warning) 8%, transparent); border-left: 2px solid var(--wg-warning); }
  .fault-bg { background: color-mix(in srgb, var(--wg-error) 8%, transparent); border-left: 2px solid var(--wg-error); }
  .bp-bg { background: color-mix(in srgb, var(--wg-warning) 8%, transparent); border-left: 2px solid var(--wg-warning); }
  .port-val { white-space: pre-wrap; word-break: break-all; }
  .focus-ring:focus { border-color: var(--wg-primary); outline: none; box-shadow: 0 0 0 2px color-mix(in srgb, var(--wg-primary) 25%, transparent); }
  .section-accent { border-left: 2px solid var(--wg-primary); padding-left: 8px; }
  .section-accent-in { border-left-color: var(--wg-primary); }
  .section-accent-out { border-left-color: var(--wg-success); }
  .port-card { transition: background-color 0.1s; }
  .port-card:hover { background-color: color-mix(in srgb, var(--wg-surface) 80%, transparent); }
  .arrow { color: var(--wg-text-muted); font-size: 1.5rem; line-height: 1; }
  .toggle-track { position: relative; width: 36px; height: 20px; border-radius: var(--_toggle-radius);
    background: var(--wg-surface-hover); cursor: pointer; transition: background 0.2s; }
  .toggle-track.on { background: var(--wg-primary); }
  .toggle-knob { position: absolute; top: 2px; left: 2px; width: 16px; height: 16px;
    border-radius: 50%; background: var(--wg-text); transition: left 0.2s; }
  .toggle-track.on .toggle-knob { left: 18px; }
`;

/**
 * What a built inspector shows: its markup, and the values of its form
 * controls (which the markup leaves out). Equal for equal content.
 */
function signature(root: Element): string {
  const values = [...root.querySelectorAll('input, textarea, select')].map((control) => {
    const c = control as HTMLInputElement;
    return c.type === 'checkbox' ? String(c.checked) : c.value;
  });
  return [root.outerHTML, ...values].join('\u0000');
}

/** Whether `el` takes typing: a text field, a text area or a list. */
function isEditable(el: Element | null): boolean {
  if (!el) return false;
  const tag = el.tagName;
  return (
    tag === 'TEXTAREA' ||
    tag === 'SELECT' ||
    (tag === 'INPUT' && (el as HTMLInputElement).type !== 'checkbox')
  );
}

export class WitgraphInspector extends HTMLElement {
  #controller: EditorController | null = null;
  // Not on `output-changed`: stream items are previewed in the node, and
  // re-rendering for each would reset whatever is being typed here.
  #onStateChanged = () => this.render();
  /** What is shown: its {@link signature}, and what it inspects. */
  #shown: { signature: string; subject: string } | null = null;
  /** A render put off while a field here was being edited. */
  #deferred = false;

  constructor() {
    super();
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);
    // A render put off for a field being edited happens once it is left.
    shadow.addEventListener('focusout', () => {
      if (!this.#deferred) return;
      this.#deferred = false;
      queueMicrotask(() => this.render());
    });
  }

  set controller(ctrl: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.render();
  }

  connectedCallback(): void {
    this.render();
  }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  /**
   * Shows the selection's details. Nothing on screen is touched when they
   * are what is shown already, so a running graph's many state changes do
   * not redraw it; and a field being edited is not swapped out from under
   * the cursor while what it belongs to is still selected.
   */
  render(): void {
    const root = this.shadowRoot!;
    const subject = `${this.#controller?.selectedEdge ?? ''}\u0000${this.#controller?.selectedNode ?? ''}`;
    const next = this.#build();
    const shown = { signature: signature(next), subject };
    const old = root.querySelector('.inspector-root');
    if (old && this.#shown?.signature === shown.signature) return;
    if (old && this.#shown?.subject === subject && isEditable(root.activeElement)) {
      this.#deferred = true;
      return;
    }
    const scroll = this.scrollTop;
    if (old) old.replaceWith(next);
    else root.appendChild(next);
    if (this.#shown?.subject === subject) this.scrollTop = scroll;
    this.#shown = shown;
  }

  /** The inspector for the selected edge or node, or else the graph's properties. */
  #build(): HTMLElement {
    const built = this.#buildSelection();
    // What changes the graph waits while it runs (see `EditorController.editable`).
    // A pane with none of it is left as it is.
    const structural = built.querySelectorAll<HTMLElement>('[data-structural]');
    if (this.#controller && !this.#controller.editable && structural.length > 0) {
      built.classList.add('locked');
      for (const el of structural) {
        if (
          el instanceof HTMLInputElement ||
          el instanceof HTMLSelectElement ||
          el instanceof HTMLButtonElement
        )
          el.disabled = true;
        el.title = 'Stop the run to edit the graph';
      }
    }
    return built;
  }

  #buildSelection(): HTMLElement {
    const controller = this.#controller;
    if (!controller) return this.#buildGraphProperties();
    const edgeId = controller.selectedEdge;
    const edge = edgeId ? controller.edges.find((e) => e.id === edgeId) : undefined;
    if (edge) return this.#buildEdgeInspector(edge);
    const nodeId = controller.selectedNode;
    const node = nodeId ? controller.nodes.get(nodeId) : undefined;
    return node ? this.#buildInspector(node) : this.#buildGraphProperties();
  }

  /**
   * A Value output as WAVE text, or a summary of it when it is too big to
   * write out (its text could take seconds, and hundreds of megabytes).
   */
  #outputText(node: string, port: string): string {
    const value = this.#controller?.latestOutput(node, port);
    const visual = this.#controller?.nodes.get(node);
    const output = visual?.outputs.find((p) => p.name === port);
    // What moves to its reader has no value to read: say what it is doing instead.
    if (output?.kind === 'stream' || output?.kind === 'future') {
      if (visual?.phase === 'pending') return '-';
      const running = visual?.phase === 'running';
      const doing =
        output.kind === 'stream'
          ? running
            ? 'streaming'
            : 'stream ended'
          : running
            ? 'waiting'
            : 'settled';
      return value === undefined ? doing : `${doing}, latest: ${summarize(value)}`;
    }
    if (value === undefined) return '-';
    const handle = output?.handle;
    // A handle has no text: its object may describe itself.
    if (handle) return describeHandle(value, handle.resource);
    if (weight(value) > MAX_WAVE_BYTES) return `${summarize(value)} (too large to show as WAVE)`;
    return this.#controller?.outputWave(node, port) ?? '…';
  }

  /** What compiling says of what is shown, if anything. */
  #appendDiagnostics(wrap: HTMLElement, diagnostics: readonly Diagnostic[]): void {
    // What stops the graph comes before what does not.
    const worstFirst = [...diagnostics].sort(
      (a, b) => Number(b.severity === 'error') - Number(a.severity === 'error'),
    );
    for (const diagnostic of worstFirst) {
      const { severity, message, help } = diagnostic;
      const settled = this.#controller?.isSettled(diagnostic) ?? false;
      const look = settled ? 'settled-bg' : severity === 'error' ? 'fault-bg' : 'warning-bg';
      const entry = this.#el('div', `${look} rounded-md p-3`);
      entry.appendChild(
        this.#el(
          'div',
          'font-mono text-xs whitespace-pre-wrap wrap-anywhere text-wg-text',
          message,
        ),
      );
      if (help) entry.appendChild(this.#el('div', 'text-wg-text-dim text-xs mt-1', help));
      if (settled) {
        entry.appendChild(
          this.#el('div', 'text-wg-text-dim text-xs mt-1', 'Settled: a value is injected.'),
        );
      }
      wrap.appendChild(entry);
    }
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
    const meta = (graph?.metadata ?? {}) as {
      name?: string;
      description?: string;
      attrs?: Record<string, string>;
    };

    const titleBar = this.#el(
      'div',
      'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none mb-3',
    );
    titleBar.appendChild(this.#el('span', 'text-lg', '\u{1F4CA}'));
    titleBar.appendChild(this.#el('span', '', 'Graph Properties'));
    wrap.appendChild(titleBar);

    // Name
    const nameRow = this.#el('div', 'mb-2');
    nameRow.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Name'));
    const nameInput = document.createElement('input');
    nameInput.type = 'text';
    nameInput.className =
      'label-input w-full bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 text-sm';
    nameInput.value = meta.name ?? '';
    nameInput.addEventListener('change', () => this.#controller?.setGraphName(nameInput.value));
    nameRow.appendChild(nameInput);
    wrap.appendChild(nameRow);

    // Description
    const descRow = this.#el('div', 'mb-2');
    descRow.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Description'));
    const descInput = document.createElement('textarea');
    descInput.className =
      'label-input w-full bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 text-sm';
    descInput.rows = 3;
    descInput.value = meta.description ?? '';
    descInput.addEventListener('change', () =>
      this.#controller?.setGraphDescription(descInput.value),
    );
    descRow.appendChild(descInput);
    wrap.appendChild(descRow);

    // Stats
    const stats = this.#el('div', 'mb-3 p-2 bg-wg-surface rounded-md');
    stats.appendChild(this.#el('div', 'text-xs text-wg-text-dim mb-1', 'Statistics'));
    stats.appendChild(this.#el('div', 'text-sm', `${this.#controller?.nodes.size ?? 0} nodes`));
    stats.appendChild(this.#el('div', 'text-sm', `${this.#controller?.edges.length ?? 0} edges`));
    const groups = this.#controller?.streamGroups ?? [];
    stats.appendChild(
      this.#el('div', 'text-sm', `${groups.length} stream group${groups.length !== 1 ? 's' : ''}`),
    );
    if (groups.length > 0) {
      const groupList = this.#el('div', 'mt-1');
      groups.forEach((members, i) => {
        const row = this.#el('div', 'font-mono text-xs text-wg-text-dim py-0.5');
        row.textContent = `  🔗 #${i}: ${members.join(', ')}`;
        groupList.appendChild(row);
      });
      stats.appendChild(groupList);
      stats.appendChild(
        this.#el(
          'div',
          'text-xs text-wg-text-dim mt-1',
          'A stream can be read once, so nodes sharing one re-run together. Every other node runs on its own.',
        ),
      );
    }
    const compiles = this.#controller?.compiles;
    stats.appendChild(
      this.#el(
        'div',
        `text-sm mt-1 ${compiles ? 'text-wg-success' : 'text-wg-error'}`,
        compiles ? 'Compiles' : `Error: ${this.#controller?.compileError ?? 'unknown'}`,
      ),
    );
    wrap.appendChild(stats);

    wrap.appendChild(this.#buildResourcesSection());

    // Custom attrs
    wrap.appendChild(
      this.#buildAttrsEditor(
        'Graph Attributes',
        meta.attrs ?? {},
        (k, v) => this.#controller?.setGraphAttr(k, v),
        (k) => this.#controller?.removeGraphAttr(k),
      ),
    );

    // Snapshots
    wrap.appendChild(this.#buildSnapshotsSection());

    // Hint
    wrap.appendChild(
      this.#el(
        'div',
        'text-xs text-wg-text-dim mt-3 text-center',
        'Select a node or edge to inspect its details',
      ),
    );

    return wrap;
  }

  /** A small button showing an icon, as the toolbar's do. */
  #iconButton(
    icon: Parameters<typeof createElement>[0],
    title: string,
    cls: string,
    onClick: () => void,
  ): HTMLButtonElement {
    const button = document.createElement('button');
    button.className = `icon-btn ${cls}`.trim();
    button.title = title;
    button.setAttribute('aria-label', title);
    button.appendChild(createElement(icon) as unknown as SVGSVGElement);
    button.addEventListener('click', onClick);
    return button;
  }

  #buildSnapshotsSection(): HTMLElement {
    const section = this.#el('div', 'mb-3');
    const snaps = this.#controller?.savedSnapshots ?? [];
    section.appendChild(
      this.#el(
        'div',
        'font-semibold mb-1.5 text-wg-text border-b border-wg-surface pb-1',
        `Snapshots (${snaps.length})`,
      ),
    );

    if (snaps.length === 0) {
      section.appendChild(
        this.#el(
          'div',
          'text-xs text-wg-text-dim italic p-2',
          'No snapshots saved. Use the Save button in the toolbar.',
        ),
      );
    } else {
      for (const snap of snaps) {
        const row = this.#el(
          'div',
          'flex items-center gap-2 p-2 bg-wg-surface-50 rounded-md mb-1 hover:bg-wg-surface-hover transition-all',
        );
        const info = this.#el('div', 'flex-1 min-h-0');
        info.appendChild(this.#el('div', 'font-semibold text-xs truncate', snap.name));
        const timeStr = new Date(snap.timestamp).toLocaleTimeString();
        info.appendChild(this.#el('div', 'text-2xs text-wg-text-dim', timeStr));
        row.appendChild(info);

        row.appendChild(
          this.#iconButton(History, `Restore ${snap.name}`, '', () => {
            void this.#controller?.restoreNamedSnapshot(snap.name).catch(() => {});
          }),
        );
        const delBtn = this.#iconButton(Trash2, `Delete ${snap.name}`, 'danger', () =>
          this.#controller?.deleteSnapshot(snap.name),
        );
        row.appendChild(delBtn);
        section.appendChild(row);
      }
    }

    return section;
  }

  /** A number field for how much of a resource: a bad value is the controller's to refuse. */
  #sizeInput(value: number | null, placeholder: string, onChange: (size: number) => void) {
    const input = document.createElement('input');
    input.type = 'number';
    input.min = '0';
    input.step = 'any';
    input.placeholder = placeholder;
    input.className =
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    input.style.minWidth = '0';
    input.dataset.structural = '';
    if (value !== null) input.setAttribute('value', String(value));
    input.addEventListener('change', () => onChange(input.valueAsNumber));
    return input;
  }

  #resourceRow(name: string, ...cells: HTMLElement[]): HTMLElement {
    const row = document.createElement('div');
    row.className = 'resource-row';
    row.dataset.resource = name;
    row.style.cssText =
      'display:grid; grid-template-columns:1fr 1fr 4em 20px; gap:4px; margin-bottom:4px; align-items:center;';
    row.append(this.#el('span', 'text-xs font-mono text-wg-text-dim truncate', name), ...cells);
    return row;
  }

  /** A row to name a resource and say how much, with the names the graph has to pick from. */
  #resourceAdder(what: string, onAdd: (name: string, size: number) => void): HTMLElement {
    const row = document.createElement('div');
    row.className = 'resource-add';
    row.style.cssText =
      'display:grid; grid-template-columns:1fr 1fr 4em 20px; gap:4px; margin-top:4px; align-items:center;';
    const name = document.createElement('input');
    name.type = 'text';
    name.placeholder = 'resource';
    name.className =
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    name.style.minWidth = '0';
    name.dataset.structural = '';
    const names = document.createElement('datalist');
    names.id = `resource-names-${what}`;
    for (const { name: known } of this.#controller?.resources ?? []) {
      const option = document.createElement('option');
      option.value = known;
      names.appendChild(option);
    }
    name.setAttribute('list', names.id);
    const size = this.#sizeInput(null, what, () => {});
    const add = this.#el(
      'button',
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded cursor-pointer text-xs',
      '+',
    );
    add.dataset.structural = '';
    add.setAttribute('aria-label', `Add ${what}`);
    add.addEventListener('click', () => {
      if (name.value.trim() === '') return;
      onAdd(name.value, size.valueAsNumber);
    });
    row.append(name, size, document.createElement('span'), add, names);
    return row;
  }

  #removeResourceButton(label: string, onRemove: () => void): HTMLElement {
    const remove = this.#el('button', 'text-wg-error cursor-pointer text-xs', '\u00d7');
    remove.dataset.structural = '';
    remove.setAttribute('aria-label', label);
    remove.addEventListener('click', onRemove);
    return remove;
  }

  /** How much a node claims of each resource. */
  #buildClaimsSection(node: NodeVisual): HTMLElement {
    const section = this.#el('div', 'resource-claims mb-3');
    section.appendChild(
      this.#el(
        'div',
        'font-semibold mb-1.5 text-wg-text border-b border-wg-surface pb-1 text-xs',
        'Resources',
      ),
    );
    section.appendChild(
      this.#el(
        'div',
        'text-2xs text-wg-text-dim mb-1.5',
        'Waits to start until this much is free. It does not limit what the node uses.',
      ),
    );
    const controller = this.#controller;
    for (const [name, { amount }] of Object.entries(controller?.claimsOf(node.id) ?? {})) {
      const capacity = controller?.resources.find((use) => use.name === name)?.capacity ?? 1;
      section.appendChild(
        this.#resourceRow(
          name,
          this.#sizeInput(amount, 'amount', (size) =>
            controller?.setNodeClaim(node.id, name, size),
          ),
          this.#el('span', 'text-2xs text-wg-text-dim', `of ${capacity}`),
          this.#removeResourceButton(`Stop claiming ${name}`, () =>
            controller?.setNodeClaim(node.id, name, null),
          ),
        ),
      );
    }
    section.appendChild(
      this.#resourceAdder('amount', (name, size) => controller?.setNodeClaim(node.id, name, size)),
    );
    return section;
  }

  /** How much there is of each resource of the graph. */
  #buildResourcesSection(): HTMLElement {
    const section = this.#el('div', 'resource-capacities mb-3');
    section.appendChild(
      this.#el(
        'div',
        'font-semibold mb-1.5 text-wg-text border-b border-wg-surface pb-1 text-xs',
        'Resources',
      ),
    );
    section.appendChild(
      this.#el(
        'div',
        'text-2xs text-wg-text-dim mb-1.5',
        'How much of each can be held at once. Nodes claim it in the same units.',
      ),
    );
    const controller = this.#controller;
    for (const { name, capacity, declared, claimed } of controller?.resources ?? []) {
      section.appendChild(
        this.#resourceRow(
          name,
          this.#sizeInput(capacity, 'capacity', (size) =>
            controller?.setResourceCapacity(name, size),
          ),
          this.#el('span', 'text-2xs text-wg-text-dim', `${claimed} claimed`),
          declared
            ? this.#removeResourceButton(`Use the default capacity of ${name}`, () =>
                controller?.setResourceCapacity(name, null),
              )
            : document.createElement('span'),
        ),
      );
    }
    section.appendChild(
      this.#resourceAdder('capacity', (name, size) => controller?.setResourceCapacity(name, size)),
    );
    return section;
  }

  #buildAttrsEditor(
    title: string,
    attrs: Record<string, string>,
    onSet: (key: string, value: string) => void,
    onRemove: (key: string) => void,
  ): HTMLElement {
    const section = this.#el('div', 'mb-3');
    section.appendChild(
      this.#el('div', 'font-semibold mb-1.5 text-wg-text border-b border-wg-surface pb-1', title),
    );

    const editorAttrs = Object.entries(attrs).filter(
      ([k]) => !k.startsWith('editor.') && !k.startsWith('inject.'),
    );
    for (const [key, val] of editorAttrs) {
      const row = document.createElement('div');
      row.style.cssText =
        'display:grid; grid-template-columns:1fr 1fr 20px; gap:4px; margin-bottom:4px; align-items:center;';
      const keySpan = this.#el('span', 'text-xs font-mono text-wg-text-dim truncate', key);
      row.appendChild(keySpan);
      const valInput = document.createElement('input');
      valInput.type = 'text';
      valInput.className =
        'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
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
    addRow.style.cssText =
      'display:grid; grid-template-columns:1fr 1fr 20px; gap:4px; margin-top:4px; align-items:center;';
    const keyInput = document.createElement('input');
    keyInput.type = 'text';
    keyInput.placeholder = 'key';
    keyInput.className =
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    keyInput.style.minWidth = '0';
    const valInput = document.createElement('input');
    valInput.type = 'text';
    valInput.placeholder = 'value';
    valInput.className =
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
    valInput.style.minWidth = '0';
    const addBtn = this.#el(
      'button',
      'bg-wg-surface text-wg-text border border-wg-text-dim rounded cursor-pointer text-xs',
      '+',
    );
    addBtn.addEventListener('click', () => {
      if (keyInput.value) {
        onSet(keyInput.value, valInput.value);
        keyInput.value = '';
        valInput.value = '';
      }
    });
    addRow.append(keyInput, valInput, addBtn);
    section.appendChild(addRow);

    return section;
  }

  #buildInspector(node: NodeVisual): HTMLDivElement {
    const wrap = this.#el('div', 'inspector-root p-3 flex flex-col gap-3') as HTMLDivElement;

    // Header
    const header = this.#el(
      'div',
      'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none',
    );
    header.appendChild(this.#el('span', 'text-lg', '\u{1F4E6}'));
    header.appendChild(this.#el('span', 'flex-1', 'Node'));
    header.appendChild(
      this.#el(
        'span',
        `px-2 py-1 rounded-md text-xs font-bold text-wg-bg shadow-sm ${PHASE_BG[node.phase] ?? ''}`,
        node.phase.toUpperCase(),
      ),
    );
    wrap.appendChild(header);

    // Editable node ID
    const idRow = this.#el('div', 'flex items-center gap-2');
    idRow.appendChild(this.#el('span', 'text-xs text-wg-text-dim font-semibold select-none', 'ID'));
    const idInput = document.createElement('input');
    idInput.type = 'text';
    idInput.value = node.id;
    idInput.className =
      'node-id focus-ring flex-1 bg-wg-surface text-wg-text border border-wg-text-dim rounded-md px-2 py-1 font-mono text-sm';
    idInput.style.minWidth = '0';
    idInput.dataset.structural = '';
    idInput.addEventListener('change', () => {
      const newId = idInput.value.trim();
      if (newId && newId !== node.id) this.#controller?.renameNode(node.id, newId);
    });
    idRow.appendChild(idInput);
    wrap.appendChild(idRow);

    // Component info
    const graphNode = (
      (this.#controller?.graphJson as Record<string, unknown>)?.nodes as Array<{
        id: string;
        component: Record<string, unknown>;
        attrs?: Record<string, string>;
      }>
    )?.find((n) => n.id === node.id);
    if (graphNode?.component) {
      const comp = graphNode.component as {
        package?: { namespace?: string; name?: string; version?: string };
        world?: string;
      };
      const compInfo = this.#el('div', 'p-2 bg-wg-surface rounded-md text-xs');
      compInfo.appendChild(this.#el('div', 'text-wg-text-dim mb-1', 'Component'));
      const compId = [
        comp.package?.namespace,
        ':',
        comp.package?.name,
        comp.world ? `/${comp.world}` : '',
        comp.package?.version ? `@${comp.package.version}` : '',
      ]
        .filter(Boolean)
        .join('');
      compInfo.appendChild(this.#el('div', 'font-mono text-wg-primary truncate', compId));
      if (node.docs)
        compInfo.appendChild(this.#el('div', 'node-docs text-wg-text mt-1', node.docs));
      wrap.appendChild(compInfo);
    }

    // Label editor
    const labelRow = this.#el('div', 'flex items-center gap-2');
    labelRow.appendChild(
      this.#el('span', 'text-xs text-wg-text-dim font-semibold select-none', 'Label'),
    );
    const labelInput = document.createElement('input');
    labelInput.type = 'text';
    labelInput.className =
      'focus-ring flex-1 bg-wg-surface text-wg-text border border-wg-text-dim rounded-md px-2 py-1 text-sm';
    labelInput.value = node.label;
    labelInput.placeholder = 'Node label...';
    labelInput.addEventListener('change', () =>
      this.#controller?.setNodeLabel(node.id, labelInput.value),
    );
    labelRow.appendChild(labelInput);
    wrap.appendChild(labelRow);

    // Breakpoint inputs
    const bp = this.#controller?.breakpointState;
    if (bp && bp.node === node.id) {
      const section = this.#el('div', 'bp-bg rounded-md p-3');
      section.appendChild(
        this.#el(
          'div',
          'font-semibold text-wg-warning mb-2 text-sm',
          '⏸ Paused before this node runs - Inputs',
        ),
      );
      for (const [key, val] of Object.entries(bp.inputs)) {
        const entry = this.#el('div', 'flex items-center gap-2 font-mono text-xs py-0.5');
        entry.appendChild(this.#el('span', 'text-wg-text font-semibold', key));
        // As an output with no value, or one too large, is shown.
        const shown =
          val === undefined
            ? '-'
            : val.length > MAX_WAVE_BYTES
              ? `${val.slice(0, 80)}… (too large to show whole)`
              : val;
        entry.appendChild(
          this.#el(
            'span',
            'text-wg-primary bg-wg-primary-5 px-1.5 py-0.5 rounded wrap-anywhere',
            shown,
          ),
        );
        section.appendChild(entry);
      }
      wrap.appendChild(section);
    }

    // Fault
    if (node.fault) {
      const fault = this.#el('div', 'fault-bg rounded-md p-3');
      fault.appendChild(
        this.#el('div', 'font-semibold text-wg-error text-sm mb-1', `⚠ ${node.fault.kind}`),
      );
      fault.appendChild(
        this.#el(
          'div',
          'font-mono text-xs whitespace-pre-wrap wrap-anywhere text-wg-text',
          node.fault.message,
        ),
      );
      if (node.culprit)
        fault.appendChild(
          this.#el('div', 'text-wg-text-dim text-xs mt-1', `Culprit: ${node.culprit}`),
        );
      wrap.appendChild(fault);
    }

    // What compiling says of it
    this.#appendDiagnostics(wrap, this.#controller?.diagnosticsForNode(node.id) ?? []);

    // Ports
    if (node.inputs.length > 0)
      wrap.appendChild(this.#buildPortSection('Inputs', node.inputs, node, 'input'));
    if (node.outputs.length > 0) {
      wrap.appendChild(this.#buildPortSection('Outputs', node.outputs, node, 'output'));
      wrap.appendChild(this.#buildPreviewSelector(node, graphNode?.attrs));
    }

    // Capabilities
    const caps = this.#controller?.nodeCapabilities(node.id) ?? [];
    if (caps.length > 0) {
      wrap.appendChild(this.#buildCapabilitiesSection(caps));
    }

    wrap.appendChild(this.#buildClaimsSection(node));

    // Mock mode
    wrap.appendChild(this.#buildMockSection(node));

    // Custom attrs
    const gNode = (
      (this.#controller?.graphJson as Record<string, unknown>)?.nodes as Array<{
        id: string;
        attrs?: Record<string, string>;
      }>
    )?.find((n) => n.id === node.id);
    if (gNode) {
      wrap.appendChild(
        this.#buildAttrsEditor(
          'Node Attributes',
          gNode.attrs ?? {},
          (k, v) => this.#controller?.setNodeAttr(node.id, k, v),
          (k) => this.#controller?.removeNodeAttr(node.id, k),
        ),
      );
    }

    return wrap;
  }

  /** Which output the node previews: its component's choice, none, or one picked. */
  #buildPreviewSelector(node: NodeVisual, attrs: Record<string, string> = {}): HTMLElement {
    const row = this.#el('div', 'flex items-center gap-2');
    row.appendChild(
      this.#el('span', 'text-xs text-wg-text-dim font-semibold select-none', 'Preview'),
    );
    const select = document.createElement('select');
    select.className =
      'preview-select focus-ring flex-1 bg-wg-surface text-wg-text border border-wg-text-dim rounded-md px-2 py-1 text-sm';
    select.style.minWidth = '0';
    const declared = node.outputs.find((p) => p.viewer !== undefined);
    const choices: [string, string][] = [
      ['', declared ? `Default (${declared.name})` : 'Default (none)'],
      ['none', 'None'],
      ...node.outputs.map((p): [string, string] => [
        p.name,
        `${p.name} (${p.viewer ?? defaultViewer(p)})`,
      ]),
    ];
    const current = attrs['editor.preview'] ?? '';
    for (const [value, text] of choices) {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = text;
      option.selected = value === current;
      select.appendChild(option);
    }
    select.addEventListener('change', () =>
      this.#controller?.setPreview(node.id, select.value === '' ? null : select.value),
    );
    row.appendChild(select);
    return row;
  }

  #buildCapabilitiesSection(
    caps: readonly { interface: string; implements?: string; satisfied: boolean }[],
  ): HTMLElement {
    const section = this.#el('div', '');
    const header = this.#el('div', 'section-accent font-semibold text-sm mb-2');
    header.textContent = `Capabilities (${caps.length})`;
    section.appendChild(header);

    for (const cap of caps) {
      const row = this.#el('div', 'flex items-center gap-2 p-2 bg-wg-surface-50 rounded-md mb-1');
      const status = this.#el(
        'span',
        `text-sm ${cap.satisfied ? 'text-wg-success' : 'text-wg-error'}`,
        cap.satisfied ? '✓' : '✗',
      );
      row.appendChild(status);
      const nameEl = this.#el('div', 'flex-1 min-h-0');
      nameEl.appendChild(this.#el('div', 'font-mono text-xs truncate', cap.interface));
      if (cap.implements) {
        nameEl.appendChild(
          this.#el('div', 'font-mono text-2xs text-wg-text-dim truncate', `→ ${cap.implements}`),
        );
      }
      row.appendChild(nameEl);
      const badge = this.#el(
        'span',
        `text-2xs font-semibold px-1.5 py-0.5 rounded ${cap.satisfied ? 'bg-wg-primary-10 text-wg-success' : 'bg-wg-error-10 text-wg-error'}`,
        cap.satisfied ? 'provided' : 'missing',
      );
      row.appendChild(badge);
      section.appendChild(row);
    }

    return section;
  }

  #buildMockSection(node: NodeVisual): HTMLElement {
    const section = this.#el('div', '');
    const header = this.#el('div', 'flex items-center gap-2 mb-2');
    header.appendChild(this.#el('span', 'section-accent font-semibold text-sm', 'Mock Mode'));

    const isMocked = this.#controller?.isMocked(node.id) ?? false;
    const track = this.#el('div', `toggle-track ${isMocked ? 'on' : ''}`);
    track.appendChild(this.#el('div', 'toggle-knob'));
    track.dataset.structural = '';
    track.addEventListener('click', () => this.#controller?.toggleMock(node.id));
    header.appendChild(track);
    section.appendChild(header);

    if (isMocked) {
      section.appendChild(
        this.#el(
          'div',
          'text-2xs text-wg-text-dim mb-2 italic',
          'WASM execution replaced — configure mock outputs below',
        ),
      );
      const mockOutputs = this.#controller?.getMockOutputs(node.id) ?? new Map();
      for (const port of node.outputs) {
        const card = this.#el('div', 'p-2 bg-wg-surface-50 rounded-md mb-1.5');
        const topRow = this.#el('div', 'flex items-center gap-1.5 mb-1');
        topRow.appendChild(this.#el('span', 'font-semibold text-xs', port.name));
        topRow.appendChild(
          this.#el('span', 'text-wg-text-dim font-mono text-2xs', port.typeDisplay),
        );
        card.appendChild(topRow);

        const form = this.#el('div', 'flex gap-1.5 items-center');
        const input = document.createElement('input');
        input.type = 'text';
        input.className =
          'focus-ring flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-2 py-1 font-mono text-xs';
        input.style.minWidth = '0';
        input.placeholder = 'WAVE value…';
        input.value = mockOutputs.get(port.name) ?? '';

        const setBtn = this.#el(
          'button',
          'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 cursor-pointer text-xs hover:bg-wg-surface-hover transition-all shadow-sm',
          'Set',
        );
        setBtn.dataset.structural = '';
        setBtn.addEventListener('click', () => {
          if (input.value) this.#controller?.setMockOutput(node.id, port.name, input.value);
          else this.#controller?.clearMockOutput(node.id, port.name);
        });
        form.append(input, setBtn);
        card.appendChild(form);
        section.appendChild(card);
      }
    }

    return section;
  }

  #buildPortSection(
    title: string,
    ports: PortVisual[],
    node: NodeVisual,
    direction: 'input' | 'output',
  ): HTMLElement {
    const section = this.#el('div', '');
    const header = this.#el(
      'div',
      `section-accent ${direction === 'input' ? 'section-accent-in' : 'section-accent-out'} font-semibold text-sm mb-2`,
    );
    header.textContent = `${title} (${ports.length})`;
    section.appendChild(header);

    for (const port of ports) {
      const card = this.#el('div', 'port-card bg-wg-surface-50 rounded-md p-2 mb-1.5 shadow-sm');
      const topRow = this.#el('div', 'flex items-center gap-1.5');
      topRow.appendChild(
        this.#el(
          'span',
          `inline-block px-1.5 py-0.5 rounded text-2xs font-bold text-wg-bg uppercase ${KIND_BG[port.kind] ?? ''}`,
          port.kind,
        ),
      );
      topRow.appendChild(this.#el('span', 'font-semibold text-sm', port.name));
      topRow.appendChild(
        this.#el('span', 'text-wg-text-dim font-mono text-2xs ml-1 truncate', port.typeDisplay),
      );
      card.appendChild(topRow);

      if (direction === 'output') {
        const pre = this.#el(
          'pre',
          'port-val font-mono text-xs mt-1.5 p-2 bg-wg-bg rounded-md text-wg-primary',
        );
        try {
          pre.textContent = this.#outputText(node.id, port.name);
        } catch {
          pre.textContent = '-';
        }
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
            if (c.feedback)
              row.appendChild(this.#el('span', 'text-wg-warning text-2xs', '(feedback)'));
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
    select.className =
      'flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-1 py-0.5 font-mono text-xs';
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

    // Feedback checkbox (only when the connection, or one it could be, can have it)
    let fbCheck: HTMLInputElement | null = null;
    const asFeedback = (fromNode: string, fromPort: string) =>
      this.#controller?.canBeFeedback(fromNode, fromPort, node.id, port.name);
    const connected = conn ? asFeedback(conn.fromNode, conn.fromPort) : undefined;
    const canFeedback = conn
      ? connected?.ok
      : compatible.some((output) => asFeedback(output.node, output.port)?.ok);
    if (canFeedback) {
      const fbRow = this.#el('div', 'flex items-center gap-1 mt-1');
      fbCheck = document.createElement('input');
      fbCheck.type = 'checkbox';
      fbCheck.checked = conn?.feedback ?? false;
      fbCheck.id = `fb-${node.id}-${port.name}`;
      fbCheck.dataset.structural = '';
      if (conn) {
        fbCheck.addEventListener('change', () =>
          this.#controller?.setConnectionFeedback(conn.id, fbCheck!.checked),
        );
      }
      const fbLabel = document.createElement('label');
      fbLabel.htmlFor = fbCheck.id;
      fbLabel.className = 'text-2xs text-wg-text-dim cursor-pointer';
      fbLabel.textContent = 'Feedback (value from the previous tick)';
      fbRow.title =
        'This input gets what its source produced on the previous tick, not this one. ' +
        'A cycle needs a feedback connection somewhere in it.';
      fbRow.append(fbCheck, fbLabel);
      wrap.appendChild(fbRow);
    }

    select.dataset.structural = '';
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
    input.className =
      'focus-ring flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-2 py-1 font-mono text-xs';
    input.style.minWidth = '0';
    input.placeholder = 'WAVE value…';
    if (currentValue !== undefined) input.value = currentValue;

    const injectBtn = this.#el('button', BTN, 'Inject');
    const clearBtn = this.#el('button', `${BTN} opacity-85`, '×');
    (clearBtn as HTMLElement).title = 'Clear';
    form.append(input, injectBtn, clearBtn);

    injectBtn.addEventListener('click', async () => {
      form.parentElement?.querySelector('.inject-error')?.remove();
      try {
        await this.#controller!.injectValue(node.id, port.name, input.value);
      } catch (err: unknown) {
        const errEl = this.#el(
          'div',
          'inject-error text-wg-error text-xs mt-1 p-1 bg-wg-error-10 rounded',
          err instanceof Error ? err.message : String(err),
        );
        form.parentElement!.appendChild(errEl);
      }
    });

    clearBtn.addEventListener('click', () => {
      void this.#controller!.clearInput(node.id, port.name).catch(() => {});
    });

    wrap.appendChild(form);
    return wrap;
  }

  #buildEdgeInspector(edge: {
    id: string;
    fromNode: string;
    fromPort: string;
    toNode: string;
    toPort: string;
    feedback: boolean;
  }): HTMLDivElement {
    const wrap = this.#el('div', 'inspector-root p-3 flex flex-col gap-3') as HTMLDivElement;
    this.#appendDiagnostics(wrap, this.#controller?.diagnosticsForConnection(edge.id) ?? []);

    const edgeHeader = this.#el(
      'div',
      'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none',
    );
    edgeHeader.appendChild(this.#el('span', 'text-lg', '\u{1F517}'));
    edgeHeader.appendChild(this.#el('span', '', 'Edge'));
    wrap.appendChild(edgeHeader);
    wrap.appendChild(
      this.#el(
        'div',
        'font-mono text-xs text-wg-text bg-wg-surface p-2 rounded-md shadow-sm truncate',
        edge.id,
      ),
    );

    // From → To with arrow
    const flow = this.#el('div', 'flex items-center gap-3 p-3 bg-wg-surface rounded-md shadow-sm');
    const fromBox = this.#el('div', 'flex-1 text-center');
    fromBox.appendChild(
      this.#el('div', 'text-2xs text-wg-text-dim uppercase font-semibold mb-0.5', 'From'),
    );
    fromBox.appendChild(this.#el('div', 'font-bold text-sm', edge.fromNode));
    fromBox.appendChild(this.#el('div', 'font-mono text-xs text-wg-primary', edge.fromPort));
    flow.appendChild(fromBox);
    flow.appendChild(this.#el('div', 'arrow select-none', '→'));
    const toBox = this.#el('div', 'flex-1 text-center');
    toBox.appendChild(
      this.#el('div', 'text-2xs text-wg-text-dim uppercase font-semibold mb-0.5', 'To'),
    );
    toBox.appendChild(this.#el('div', 'font-bold text-sm', edge.toNode));
    toBox.appendChild(this.#el('div', 'font-mono text-xs text-wg-primary', edge.toPort));
    flow.appendChild(toBox);
    wrap.appendChild(flow);

    // Feedback toggle (Value ports only)
    const srcPort = this.#controller?.nodes
      .get(edge.fromNode)
      ?.outputs.find((p) => p.name === edge.fromPort);
    const asFeedback = this.#controller?.canBeFeedback(
      edge.fromNode,
      edge.fromPort,
      edge.toNode,
      edge.toPort,
    );

    if (asFeedback?.ok) {
      const fbRow = this.#el('div', 'flex items-center gap-3 p-2 bg-wg-surface rounded-md');
      const track = this.#el('div', `toggle-track ${edge.feedback ? 'on' : ''}`);
      track.appendChild(this.#el('div', 'toggle-knob'));
      track.dataset.structural = '';
      track.addEventListener('click', () => {
        this.#controller?.setConnectionFeedback(edge.id, !edge.feedback);
      });
      fbRow.appendChild(track);
      const fbText = this.#el('div', '');
      fbText.appendChild(this.#el('div', 'text-sm font-semibold text-wg-text', 'Feedback'));
      fbText.appendChild(
        this.#el('div', 'text-2xs text-wg-text-dim', 'Unit delay - delivers previous tick value'),
      );
      fbRow.appendChild(fbText);
      wrap.appendChild(fbRow);
    } else {
      wrap.appendChild(
        this.#el(
          'div',
          'text-xs text-wg-text-dim italic p-2 bg-wg-surface rounded-md',
          `A ${srcPort?.kind ?? 'connection'} connection cannot be feedback.`,
        ),
      );
    }

    // Current value
    wrap.appendChild(this.#el('div', 'section-accent font-semibold text-sm', 'Current Value'));
    const valEl = this.#el(
      'pre',
      'port-val font-mono text-xs p-2 bg-wg-surface rounded-md text-wg-primary shadow-sm',
    );
    try {
      valEl.textContent = this.#outputText(edge.fromNode, edge.fromPort);
    } catch {
      valEl.textContent = '-';
    }
    wrap.appendChild(valEl);

    return wrap;
  }
}

if (!customElements.get('witgraph-inspector'))
  customElements.define('witgraph-inspector', WitgraphInspector);
