import type { EditorController } from '../controller/editor-controller.ts';
import type { TraceEntry } from '../controller/types.ts';
import { adoptStyles } from './shared-styles.ts';

function eventColor(event: string): string {
  switch (event) {
    case 'run-started':
    case 'run-returned':
      return 'var(--wg-primary)';
    case 'phase-transition':
      return 'var(--wg-primary)';
    case 'fault':
    case 'error':
      return 'var(--wg-error)';
    case 'cancelled':
    case 'generation-stopped':
      return 'var(--wg-accent)';
    case 'restarted':
    case 'warning':
      return 'var(--wg-warning)';
    case 'compile-ok':
      return 'var(--wg-success)';
    default:
      return 'var(--wg-text-dim)';
  }
}

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow: hidden; }
  .event { padding: 3px 8px; border-left: 3px solid var(--wg-text-dim); margin-bottom: 2px; }
  .event-error { background: color-mix(in srgb, var(--wg-error) 10%, transparent); border-radius: 3px; }
  .event-error .ev-name { color: var(--wg-error); }
  .event-error .ev-detail { color: var(--wg-error); opacity: 0.9; }
  .events-wrap::-webkit-scrollbar { width: 6px; }
  .events-wrap::-webkit-scrollbar-track { background: var(--wg-bg); }
  .events-wrap::-webkit-scrollbar-thumb { background: var(--wg-surface-hover); border-radius: 3px; }
  .events-wrap::-webkit-scrollbar-thumb:hover { background: var(--wg-overlay); }
`;

export class WitgraphTimeline extends HTMLElement {
  #controller: EditorController | null = null;
  #lastRendered: TraceEntry | null = null;
  #eventsContainer: HTMLDivElement | null = null;

  #onTraceEvent = () => this.#sync();
  #onStateChanged = () => this.#sync();

  connectedCallback(): void {
    if (this.shadowRoot) return;
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);

    const timeline = document.createElement('div');
    timeline.className = 'flex flex-col h-full bg-wg-bg';

    const header = document.createElement('div');
    header.className = 'px-3 py-1.5 font-semibold bg-wg-surface border-b border-wg-bg';
    header.textContent = 'Timeline';
    timeline.appendChild(header);

    this.#eventsContainer = document.createElement('div');
    this.#eventsContainer.className = 'events-wrap flex-1 overflow-y-auto p-1';
    timeline.appendChild(this.#eventsContainer);

    shadow.appendChild(timeline);
  }

  disconnectedCallback(): void {
    this.#detach();
  }

  set controller(ctrl: EditorController) {
    this.#detach();
    this.#controller = ctrl;
    this.#lastRendered = null;
    if (this.#eventsContainer) this.#eventsContainer.innerHTML = '';
    ctrl.addEventListener('trace-event', this.#onTraceEvent);
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.#sync();
  }

  get controller(): EditorController | null {
    return this.#controller;
  }

  #detach(): void {
    this.#controller?.removeEventListener('trace-event', this.#onTraceEvent);
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  #sync(): void {
    if (!this.#controller || !this.#eventsContainer) return;
    const events = this.#controller.traceEvents;
    const newest = events.at(-1) ?? null;
    if (newest === this.#lastRendered) return;

    // A full trace drops its oldest entries, so its length stops changing:
    // what is new is whatever follows the last entry shown.
    const shown = this.#lastRendered ? events.lastIndexOf(this.#lastRendered) : -1;
    // Not there: the trace started over, or has moved on past every row.
    if (shown === -1) this.#eventsContainer.innerHTML = '';
    for (let i = shown + 1; i < events.length; i++) {
      this.#eventsContainer.appendChild(this.#createEventEl(events[i]));
    }
    while (this.#eventsContainer.childElementCount > events.length) {
      this.#eventsContainer.firstElementChild?.remove();
    }
    this.#lastRendered = newest;
    this.#eventsContainer.scrollTop = this.#eventsContainer.scrollHeight;
  }

  #createEventEl(entry: TraceEntry): HTMLDivElement {
    const el = document.createElement('div');
    el.className =
      entry.event === 'error'
        ? 'event event-error font-mono text-xs leading-snug'
        : 'event font-mono text-xs leading-snug';
    el.style.borderLeftColor = eventColor(entry.event);

    const time = document.createElement('span');
    time.className = 'text-wg-text-dim text-xs';
    time.textContent = `${(entry.timestamp / 1000).toFixed(2)}s `;
    el.appendChild(time);

    const name = document.createElement('span');
    name.className = 'ev-name font-semibold';
    name.textContent = entry.event;
    el.appendChild(name);

    // A node's event names the node; an island's names its nodes.
    const nodes =
      entry.node !== undefined
        ? [entry.node]
        : entry.island !== undefined
          ? (this.#controller?.islands[entry.island] ?? [])
          : [];
    nodes.forEach((node, i) => {
      el.appendChild(document.createTextNode(i === 0 ? ' ' : ', '));
      const nodeSpan = document.createElement('span');
      nodeSpan.className = 'text-wg-primary cursor-pointer underline';
      nodeSpan.textContent = node;
      nodeSpan.addEventListener('click', () => this.#controller?.select(node));
      el.appendChild(nodeSpan);
    });

    const details: string[] = [];
    if (entry.generation !== undefined) details.push(`(generation ${entry.generation})`);
    if (entry.message) details.push(entry.message);
    if (entry.from && entry.to) details.push(`${entry.from} → ${entry.to}`);
    if (entry.fault) details.push(entry.fault.message);

    if (details.length > 0) {
      el.appendChild(document.createTextNode(' '));
      const detail = document.createElement('span');
      detail.className = 'ev-detail text-wg-text-dim';
      detail.textContent = details.join(' — ');
      el.appendChild(detail);
    }

    return el;
  }
}

if (!customElements.get('witgraph-timeline'))
  customElements.define('witgraph-timeline', WitgraphTimeline);
