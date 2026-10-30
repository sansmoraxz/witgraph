import { adoptStyles } from './shared-styles.ts';
import type { EditorController } from '../controller/editor-controller.ts';

const KIND_BG: Record<string, string> = {
  value: 'bg-wg-port-value',
  stream: 'bg-wg-port-stream',
  future: 'bg-wg-port-future',
};

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow-y: auto; overflow-x: hidden; background-color: #1e1e2e; }
  :host::-webkit-scrollbar { width: 6px; }
  :host::-webkit-scrollbar-track { background: #1e1e2e; }
  :host::-webkit-scrollbar-thumb { background: #45475a; border-radius: 3px; }
  :host::-webkit-scrollbar-thumb:hover { background: #585b70; }
  .card { transition: border-color 0.15s, background-color 0.15s, box-shadow 0.15s; }
  .card:hover { border-color: #89b4fa; background-color: #45475a; box-shadow: 0 2px 8px rgba(0,0,0,0.35); }
  .drag-hint { opacity: 0; transition: opacity 0.15s; }
  .card:hover .drag-hint { opacity: 1; }
  .port-list { display: grid; grid-template-columns: 1fr 1fr; gap: 2px; }
`;

export class WitgraphCatalog extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.#render();

  constructor() {
    super();
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);
  }

  set controller(ctrl: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.#render();
  }

  connectedCallback(): void { this.#render(); }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  #el(tag: string, cls: string, text?: string): HTMLElement {
    const el = document.createElement(tag);
    el.className = cls;
    if (text !== undefined) el.textContent = text;
    return el;
  }

  #render(): void {
    const root = this.shadowRoot!;
    const existing = root.querySelector('.catalog-wrap');
    if (existing) existing.remove();

    const wrap = this.#el('div', 'catalog-wrap p-2 flex flex-col gap-2');

    const header = this.#el('div', 'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none');
    header.appendChild(this.#el('span', 'text-lg', '\u{1F9E9}'));
    header.appendChild(this.#el('span', '', 'Components'));
    wrap.appendChild(header);

    if (!this.#controller) {
      wrap.appendChild(this.#emptyState('No graph loaded'));
      root.appendChild(wrap);
      return;
    }

    const sources = this.#controller.componentSources;
    if (sources.length === 0) {
      wrap.appendChild(this.#emptyState('No components available'));
      root.appendChild(wrap);
      return;
    }

    for (const source of sources) {
      const card = this.#el('div', 'card p-3 bg-wg-surface rounded-lg cursor-grab border border-transparent shadow-sm');
      card.draggable = true;
      card.addEventListener('dragstart', (e) => {
        e.dataTransfer?.setData('witgraph/component-index', String(source.index));
      });

      const worldMatch = source.id.match(/\/([^@#]+)/);
      const worldName = worldMatch ? worldMatch[1] : source.id;
      card.appendChild(this.#el('div', 'font-bold text-sm mb-0.5', worldName));
      card.appendChild(this.#el('div', 'text-2xs text-wg-text-dim truncate mb-2', source.id));

      const allPorts = [...source.inputs, ...source.outputs];
      if (allPorts.length > 0) {
        const badges = this.#el('div', 'flex gap-1.5 flex-wrap mb-2');
        const kinds = new Map<string, number>();
        for (const p of allPorts) kinds.set(p.kind, (kinds.get(p.kind) ?? 0) + 1);
        for (const [kind, count] of kinds) {
          badges.appendChild(this.#el('span',
            `inline-block px-1.5 py-0.5 rounded text-2xs font-semibold text-wg-bg ${KIND_BG[kind] ?? ''}`,
            `${count} ${kind}`));
        }
        card.appendChild(badges);

        const portGrid = this.#el('div', 'port-list text-2xs text-wg-text-dim');
        const KIND_TEXT: Record<string, string> = {
          value: 'text-wg-port-value',
          stream: 'text-wg-port-stream',
          future: 'text-wg-port-future',
        };
        const inCol = this.#el('div', '');
        if (source.inputs.length > 0) {
          inCol.appendChild(this.#el('div', 'font-semibold text-wg-text mb-0.5', 'In'));
          for (const p of source.inputs) {
            const row = this.#el('div', 'flex items-center gap-1 truncate');
            row.appendChild(this.#el('span', `${KIND_TEXT[p.kind] ?? ''} text-xs`, '●'));
            row.appendChild(this.#el('span', '', p.name));
            inCol.appendChild(row);
          }
        }
        const outCol = this.#el('div', '');
        if (source.outputs.length > 0) {
          outCol.appendChild(this.#el('div', 'font-semibold text-wg-text mb-0.5', 'Out'));
          for (const p of source.outputs) {
            const row = this.#el('div', 'flex items-center gap-1 truncate');
            row.appendChild(this.#el('span', '', p.name));
            row.appendChild(this.#el('span', `${KIND_TEXT[p.kind] ?? ''} text-xs`, '●'));
            outCol.appendChild(row);
          }
        }
        portGrid.append(inCol, outCol);
        card.appendChild(portGrid);
      }

      const hint = this.#el('div', 'drag-hint text-2xs text-wg-text-dim italic text-center mt-2 select-none', 'Drag to canvas');
      card.appendChild(hint);
      wrap.appendChild(card);
    }

    root.appendChild(wrap);
  }

  #emptyState(text: string): HTMLElement {
    const el = this.#el('div', 'flex flex-col items-center gap-2 p-4 text-center text-wg-text-dim');
    el.appendChild(this.#el('div', 'text-lg', '\u{1F4E6}'));
    el.appendChild(this.#el('div', 'text-xs', text));
    return el;
  }
}

customElements.define('witgraph-catalog', WitgraphCatalog);
