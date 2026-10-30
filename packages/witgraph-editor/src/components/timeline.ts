import { adoptStyles } from './shared-styles.ts';
import type { EditorController } from '../controller/editor-controller.ts';
import type { TraceEntry } from '../controller/types.ts';

function eventColor(event: string): string {
  switch (event) {
    case 'run-started':
    case 'run-returned':
      return '#89b4fa';
    case 'phase-transition':
      return '#89b4fa';
    case 'fault':
    case 'error':
      return '#f38ba8';
    case 'cancelled':
    case 'generation-stopped':
      return '#f9e2af';
    case 'restarted':
      return '#fab387';
    case 'compile-ok':
      return '#a6e3a1';
    default:
      return '#a6adc8';
  }
}

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow: hidden; }
  .event { padding: 3px 8px; border-left: 3px solid #a6adc8; margin-bottom: 2px; }
  .event-error { background: rgba(243, 139, 168, 0.1); border-radius: 3px; }
  .event-error .ev-name { color: #f38ba8; }
  .event-error .ev-detail { color: #f38ba8; opacity: 0.9; }
  .events-wrap::-webkit-scrollbar { width: 6px; }
  .events-wrap::-webkit-scrollbar-track { background: #1e1e2e; }
  .events-wrap::-webkit-scrollbar-thumb { background: #45475a; border-radius: 3px; }
  .events-wrap::-webkit-scrollbar-thumb:hover { background: #585b70; }
`;

export class WitgraphTimeline extends HTMLElement {
  #controller: EditorController | null = null;
  #renderedCount = 0;
  #eventsContainer: HTMLDivElement | null = null;

  #onTraceEvent = () => this.#appendNew();
  #onStateChanged = () => this.#appendNew();

  connectedCallback(): void {
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
    this.#renderedCount = 0;
    if (this.#eventsContainer) this.#eventsContainer.innerHTML = '';
    ctrl.addEventListener('trace-event', this.#onTraceEvent);
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.#appendNew();
  }

  get controller(): EditorController | null { return this.#controller; }

  #detach(): void {
    this.#controller?.removeEventListener('trace-event', this.#onTraceEvent);
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  #appendNew(): void {
    if (!this.#controller || !this.#eventsContainer) return;
    const events = this.#controller.traceEvents;
    if (events.length <= this.#renderedCount) return;

    for (let i = this.#renderedCount; i < events.length; i++) {
      this.#eventsContainer.appendChild(this.#createEventEl(events[i]));
    }
    this.#renderedCount = events.length;
    this.#eventsContainer.scrollTop = this.#eventsContainer.scrollHeight;
  }

  #createEventEl(entry: TraceEntry): HTMLDivElement {
    const el = document.createElement('div');
    el.className = entry.event === 'error' ? 'event event-error font-mono text-xs leading-snug' : 'event font-mono text-xs leading-snug';
    el.style.borderLeftColor = eventColor(entry.event);

    const time = document.createElement('span');
    time.className = 'text-wg-text-dim text-xs';
    time.textContent = `${(entry.timestamp / 1000).toFixed(2)}s `;
    el.appendChild(time);

    const name = document.createElement('span');
    name.className = 'ev-name font-semibold';
    name.textContent = entry.event;
    el.appendChild(name);

    if (entry.node) {
      el.appendChild(document.createTextNode(' '));
      const nodeSpan = document.createElement('span');
      nodeSpan.className = 'text-wg-primary cursor-pointer underline';
      nodeSpan.textContent = entry.node;
      nodeSpan.addEventListener('click', () => this.#controller?.select(entry.node!));
      el.appendChild(nodeSpan);
    }

    const details: string[] = [];
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

customElements.define('witgraph-timeline', WitgraphTimeline);
