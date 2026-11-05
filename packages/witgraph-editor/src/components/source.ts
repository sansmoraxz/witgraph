import type { EditorController } from '../controller/editor-controller.ts';
import { adoptStyles } from './shared-styles.ts';

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow: hidden; }
  :host([hidden]) { display: none; }
  .panel { display: flex; flex-direction: column; height: 100%; background: var(--wg-bg); }
  .head { flex: none; display: flex; align-items: center; gap: 8px; padding: 6px 12px; background: var(--wg-surface); border-bottom: 1px solid var(--wg-bg); }
  .title { font-weight: 600; }
  .file { background: var(--wg-bg); color: var(--wg-text); border: 1px solid var(--wg-surface-hover); border-radius: 4px; max-width: 40ch; }
  .where { color: var(--wg-warning); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .rows { flex: 1 1 0; min-height: 0; overflow: auto; font-family: monospace; white-space: pre; }
  .row { display: flex; min-width: max-content; }
  .row.current { background: color-mix(in srgb, var(--wg-warning) 22%, transparent); }
  .gutter { flex: none; width: 1.4em; display: flex; align-items: center; justify-content: center; cursor: pointer; }
  .gutter:hover { background: var(--wg-surface-hover); }
  .dot { width: 0.7em; height: 0.7em; border-radius: 50%; }
  .row.breakpoint .dot { background: var(--wg-error); }
  .number { flex: none; width: 4em; padding-right: 1em; text-align: right; color: var(--wg-text-dim); }
  .row.no-code .number { opacity: 0.45; }
  .text { color: var(--wg-text); }
  .row.current .text { font-weight: 600; }
  .missing { padding: 8px; color: var(--wg-text-dim); }
  .frames { flex: none; padding: 2px 8px; color: var(--wg-text-dim); border-bottom: 1px solid var(--wg-surface-hover); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
`;

/** The end of a path, which is what tells most files apart. */
function shortName(file: string): string {
  return file.split('/').slice(-3).join('/');
}

/**
 * The source of the guests that can be stopped on a line. Its gutter sets breakpoints, and it
 * follows where a guest stops. With no such guest in the graph, it is hidden.
 */
export class WitgraphSource extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.#sync();
  #picker!: HTMLSelectElement;
  #where!: HTMLSpanElement;
  #frames!: HTMLDivElement;
  #scroller!: HTMLDivElement;
  #rows = new Map<number, HTMLDivElement>();
  /** The file on show, and what it was drawn from. */
  #file: string | null = null;
  #drawn: string | null = null;
  #filesShown = '';
  #marksShown = '';
  /** The stop last scrolled to, so a user scrolling away stays where they went. */
  #followed: unknown = null;

  connectedCallback(): void {
    if (this.shadowRoot) {
      this.#controller?.addEventListener('state-changed', this.#onStateChanged);
      this.#sync();
      return;
    }
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);

    const panel = document.createElement('div');
    panel.className = 'panel';

    const header = document.createElement('div');
    header.className = 'head';
    const title = document.createElement('span');
    title.className = 'title';
    title.textContent = 'Source';
    this.#picker = document.createElement('select');
    this.#picker.className = 'file';
    this.#picker.addEventListener('change', () => {
      this.#file = this.#picker.value;
      this.#sync();
    });
    this.#where = document.createElement('span');
    this.#where.className = 'where';
    header.append(title, this.#picker, this.#where);

    this.#frames = document.createElement('div');
    this.#frames.className = 'frames';
    this.#frames.hidden = true;

    this.#scroller = document.createElement('div');
    this.#scroller.className = 'rows';

    panel.append(header, this.#frames, this.#scroller);
    shadow.appendChild(panel);
    this.#controller?.addEventListener('state-changed', this.#onStateChanged);
    this.#sync();
  }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  set controller(controller: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = controller;
    if (this.isConnected) controller.addEventListener('state-changed', this.#onStateChanged);
    this.#sync();
  }

  get controller(): EditorController | null {
    return this.#controller;
  }

  #sync(): void {
    if (!this.#picker) return;
    const controller = this.#controller;
    const files = controller?.lineFiles ?? [];
    this.hidden = files.length === 0;
    if (!controller || files.length === 0) return;

    const stop = controller.lineStop;
    if (stop && stop !== this.#followed && files.includes(stop.file)) this.#file = stop.file;
    if (this.#file === null || !files.includes(this.#file)) this.#file = files[0];

    const listed = files.join('\n');
    if (listed !== this.#filesShown) {
      this.#filesShown = listed;
      this.#picker.replaceChildren(
        ...files.map((file) => {
          const option = document.createElement('option');
          option.value = file;
          option.textContent = shortName(file);
          option.title = file;
          return option;
        }),
      );
    }
    this.#picker.value = this.#file;

    this.#where.textContent = stop
      ? `Stopped at line ${stop.line}${stop.node ? ` in ${stop.node}` : ''}${
          stop.file === this.#file ? '' : ` of ${shortName(stop.file)}`
        }`
      : '';
    this.#frames.hidden = !stop;
    this.#frames.textContent = stop ? stop.frames.join('  ←  ') : '';
    this.#frames.title = stop ? stop.frames.join('\n') : '';

    if (this.#drawn !== this.#file) void this.#draw(this.#file);
    else this.#mark();
  }

  /** Draws a file's rows: its text if the host can say, or else only the lines with code. */
  async #draw(file: string): Promise<void> {
    this.#drawn = file;
    const controller = this.#controller;
    if (!controller) return;
    const text = await controller.sourceOf(file);
    if (this.#drawn !== file) return;

    const withCode = new Set(controller.linesOf(file));
    const lines = text?.split('\n');
    this.#rows.clear();
    this.#marksShown = '';
    const rows: HTMLElement[] = [];
    if (lines === undefined) {
      const missing = document.createElement('div');
      missing.className = 'missing';
      missing.textContent = `The source of ${file} is not available here: only its lines with code are shown.`;
      rows.push(missing);
    }
    const numbers = lines ? lines.map((_, at) => at + 1) : [...withCode];
    for (const number of numbers) {
      const row = document.createElement('div');
      row.className = withCode.has(number) ? 'row' : 'row no-code';
      row.dataset.line = String(number);
      const gutter = document.createElement('span');
      gutter.className = 'gutter';
      gutter.title = 'Stop here (or at the next line with code)';
      const dot = document.createElement('span');
      dot.className = 'dot';
      gutter.appendChild(dot);
      gutter.addEventListener('click', () => controller.toggleLineBreakpoint(file, number));
      const shown = document.createElement('span');
      shown.className = 'number';
      shown.textContent = String(number);
      const code = document.createElement('span');
      code.className = 'text';
      code.textContent = lines?.[number - 1] ?? '';
      row.append(gutter, shown, code);
      this.#rows.set(number, row);
      rows.push(row);
    }
    this.#scroller.replaceChildren(...rows);
    this.#mark();
  }

  /** Marks the breakpoints and the current line, if they moved. */
  #mark(): void {
    const controller = this.#controller;
    const file = this.#file;
    if (!controller || file === null) return;
    const stop = controller.lineStop;
    const current = stop?.file === file ? stop.line : null;
    const breakpoints = controller.lineBreakpoints(file);
    const marks = `${current}|${breakpoints.join()}`;
    if (marks !== this.#marksShown) {
      this.#marksShown = marks;
      const set = new Set(breakpoints);
      for (const [number, row] of this.#rows) {
        row.classList.toggle('breakpoint', set.has(number));
        row.classList.toggle('current', number === current);
      }
    }
    if (stop && stop !== this.#followed && current !== null) {
      this.#followed = stop;
      const row = this.#rows.get(current);
      if (row) {
        const from = this.#scroller.getBoundingClientRect();
        const at = row.getBoundingClientRect();
        this.#scroller.scrollTop += at.top - from.top - from.height / 2;
      }
    }
  }
}
