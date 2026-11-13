import type { EditorController } from '../controller/editor-controller.ts';
import { adoptStyles } from './shared-styles.ts';

const KIND_BG: Record<string, string> = {
  value: 'bg-wg-port-value',
  stream: 'bg-wg-port-stream',
  future: 'bg-wg-port-future',
  handle: 'bg-wg-port-handle',
};

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow-y: auto; overflow-x: hidden; background-color: var(--wg-bg); }
  :host::-webkit-scrollbar { width: 6px; }
  :host::-webkit-scrollbar-track { background: var(--wg-bg); }
  :host::-webkit-scrollbar-thumb { background: var(--wg-surface-hover); border-radius: 3px; }
  :host::-webkit-scrollbar-thumb:hover { background: var(--wg-overlay); }
  .card { transition: border-color 0.15s, background-color 0.15s, box-shadow 0.15s; }
  .card:hover { border-color: var(--wg-primary); background-color: var(--wg-surface-hover); box-shadow: 0 2px 8px rgba(0,0,0,0.35); }
  .drag-hint { opacity: 0; transition: opacity 0.15s; }
  .card:hover .drag-hint { opacity: 1; }
  .port-list { display: grid; grid-template-columns: 1fr 1fr; gap: 2px; }
`;

export class WitgraphCatalog extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.#render();
  /** What the cards shown were drawn from: see {@link #render}. */
  #shown: string | null = null;

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

  connectedCallback(): void {
    this.#render();
  }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  #el(tag: string, cls: string, text?: string): HTMLElement {
    const el = document.createElement(tag);
    el.className = cls;
    if (text !== undefined) el.textContent = text;
    return el;
  }

  /**
   * Shows the components. The cards are drawn from each component's id,
   * docs and ports only, so they are drawn again only when those change: not
   * on each of a running graph's many state changes.
   */
  #render(): void {
    const root = this.shadowRoot!;
    const sources = this.#controller?.componentSources;
    const shown = JSON.stringify(
      sources?.map(({ id, index, docs, inputs, outputs }) => [
        id,
        index,
        docs,
        inputs.map((p) => [p.name, p.kind]),
        outputs.map((p) => [p.name, p.kind]),
      ]) ?? null,
    );
    const existing = root.querySelector('.catalog-wrap');
    if (existing && shown === this.#shown) return;
    this.#shown = shown;
    const scroll = this.scrollTop;
    const wrap = this.#build(sources);
    if (existing) existing.replaceWith(wrap);
    else root.appendChild(wrap);
    this.scrollTop = scroll;
  }

  #build(sources: EditorController['componentSources'] | undefined): HTMLElement {
    const wrap = this.#el('div', 'catalog-wrap p-2 flex flex-col gap-2');

    const header = this.#el(
      'div',
      'flex items-center gap-2 px-2 py-2 font-semibold bg-wg-surface rounded-md shadow-sm select-none',
    );
    header.appendChild(this.#el('span', 'text-lg', '\u{1F9E9}'));
    header.appendChild(this.#el('span', '', 'Components'));
    wrap.appendChild(header);

    if (!sources) {
      wrap.appendChild(this.#emptyState('No graph loaded'));
      return wrap;
    }
    if (sources.length === 0) {
      wrap.appendChild(this.#emptyState('No components available'));
      return wrap;
    }

    for (const source of sources) {
      const card = this.#el(
        'div',
        'card p-3 bg-wg-surface rounded-lg cursor-grab border border-transparent shadow-sm',
      );
      card.draggable = true;
      card.addEventListener('dragstart', (e) => {
        // A run uses the graph: nothing is added to it until it stops.
        if (this.#controller && !this.#controller.editable) {
          e.preventDefault();
          return;
        }
        e.dataTransfer?.setData('witgraph/component-index', String(source.index));
      });

      // `namespace:name/world@version`: the world's own name, then where it is from.
      const worldName = source.world.replace(/^[^/]*\//, '').replace(/@.*$/, '');
      card.appendChild(this.#el('div', 'font-bold text-sm mb-0.5', worldName));
      const from = this.#el(
        'div',
        'text-2xs text-wg-text-dim truncate mb-2',
        `${source.component} · ${source.world}`,
      );
      from.title = source.id;
      card.appendChild(from);
      if (source.docs) {
        card.appendChild(this.#el('div', 'docs text-xs text-wg-text mb-2', source.docs));
      }

      const allPorts = [...source.inputs, ...source.outputs];
      if (allPorts.length > 0) {
        const badges = this.#el('div', 'flex gap-1.5 flex-wrap mb-2');
        const kinds = new Map<string, number>();
        for (const p of allPorts) kinds.set(p.kind, (kinds.get(p.kind) ?? 0) + 1);
        for (const [kind, count] of kinds) {
          badges.appendChild(
            this.#el(
              'span',
              `inline-block px-1.5 py-0.5 rounded text-2xs font-semibold text-wg-bg ${KIND_BG[kind] ?? ''}`,
              `${count} ${kind}`,
            ),
          );
        }
        card.appendChild(badges);

        const portGrid = this.#el('div', 'port-list text-2xs text-wg-text-dim');
        const KIND_TEXT: Record<string, string> = {
          value: 'text-wg-port-value',
          stream: 'text-wg-port-stream',
          future: 'text-wg-port-future',
          handle: 'text-wg-port-handle',
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

      const hint = this.#el(
        'div',
        'drag-hint text-2xs text-wg-text-dim italic text-center mt-2 select-none',
        'Drag to canvas',
      );
      card.appendChild(hint);
      wrap.appendChild(card);
    }

    return wrap;
  }

  #emptyState(text: string): HTMLElement {
    const el = this.#el('div', 'flex flex-col items-center gap-2 p-4 text-center text-wg-text-dim');
    el.appendChild(this.#el('div', 'text-lg', '\u{1F4E6}'));
    el.appendChild(this.#el('div', 'text-xs', text));
    return el;
  }
}

if (!customElements.get('witgraph-catalog'))
  customElements.define('witgraph-catalog', WitgraphCatalog);
