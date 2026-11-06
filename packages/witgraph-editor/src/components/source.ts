import type { BreakpointScope, LineStop, SourceVariable, StackFrame } from 'witgraph-loader';
import type { EditorController } from '../controller/editor-controller.ts';
import { ContextMenu, type MenuItem } from './context-menu.ts';
import { adoptStyles } from './shared-styles.ts';

const EXTRA_CSS = `
  :host { display: block; height: 100%; overflow: hidden; }
  :host([hidden]) { display: none; }
  .panel { display: flex; flex-direction: column; height: 100%; background: var(--wg-bg); }
  .head { flex: none; display: flex; align-items: center; gap: 8px; padding: 6px 12px; background: var(--wg-surface); border-bottom: 1px solid var(--wg-bg); }
  .title { font-weight: 600; }
  .file { background: var(--wg-bg); color: var(--wg-text); border: 1px solid var(--wg-surface-hover); border-radius: 4px; max-width: 40ch; }
  .where { color: var(--wg-warning); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .body { flex: 1 1 0; min-height: 0; display: flex; }
  .rows { flex: 3 1 0; min-width: 0; overflow: auto; font-family: monospace; white-space: pre; }
  .variables { flex: 2 1 0; min-width: 0; overflow: auto; border-left: 1px solid var(--wg-surface-hover); padding: 2px 0; }
  .variables[hidden] { display: none; }
  .variables.stale { opacity: 0.5; }
  .variables-title { padding: 2px 8px; font-weight: 600; }
  .variable { padding-left: 14px; font-family: monospace; white-space: nowrap; }
  .variable > summary { cursor: pointer; margin-left: -12px; }
  .variable-line { display: inline; }
  .variable-name { color: var(--wg-primary); }
  .variable-value { color: var(--wg-text); }
  .variable-type { color: var(--wg-text-dim); margin-left: 1ch; }
  .variable.changed > .variable-line .variable-value,
  .variable.changed > summary .variable-value { color: var(--wg-warning); font-weight: 600; }
  .no-variables { padding: 2px 8px; color: var(--wg-text-dim); }
  .row { display: flex; min-width: max-content; }
  .row.current { background: color-mix(in srgb, var(--wg-warning) 22%, transparent); }
  .gutter { flex: none; width: 1.4em; display: flex; align-items: center; justify-content: center; cursor: pointer; }
  .gutter:hover { background: var(--wg-surface-hover); }
  .dot { width: 0.7em; height: 0.7em; border-radius: 50%; }
  /* For every node running the line, as a debugger's breakpoint is: a dot. */
  .row.breakpoint .dot, .row.breakpoint-node .dot { background: var(--wg-error); }
  /* Only for the node on show: ringed. */
  .row.breakpoint-node .dot { box-shadow: 0 0 0 1.5px var(--wg-bg), 0 0 0 3px var(--wg-error); }
  /* Only for another node, so not stopping this one: hollow and faint. */
  .row.breakpoint-other .dot { box-shadow: inset 0 0 0 1.5px var(--wg-error); opacity: 0.55; }
  /* Fixed, so a menu near the panel's edge is not cut off by it. */
  .menu-layer { position: fixed; inset: 0; z-index: 100; pointer-events: none; }
  .menu-layer .ctx-menu { pointer-events: auto; white-space: nowrap; }
  .number { flex: none; width: 4em; padding-right: 1em; text-align: right; color: var(--wg-text-dim); }
  .row.no-code .number { opacity: 0.45; }
  .text { color: var(--wg-text); }
  .row.current .text { font-weight: 600; }
  .missing { padding: 8px; color: var(--wg-text-dim); }
  .frames { padding-bottom: 4px; }
  .frame { display: flex; gap: 1ch; width: 100%; text-align: left; padding: 0 8px; background: none; border: 0; color: var(--wg-text); font-family: monospace; white-space: nowrap; cursor: pointer; }
  .frame-name { flex: 1 1 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
  .frame:hover:not(:disabled) { background: var(--wg-surface-hover); }
  .frame:disabled { color: var(--wg-text-dim); cursor: default; }
  .frame.selected { background: color-mix(in srgb, var(--wg-primary) 18%, transparent); }
  .frame-at { flex: none; color: var(--wg-text-dim); }
  .row.frame-line { background: color-mix(in srgb, var(--wg-primary) 18%, transparent); }
  .caller-note { padding: 2px 8px; color: var(--wg-text-dim); }
  .caller-note[hidden] { display: none; }
`;

const GUTTER_TITLE =
  'Click: a breakpoint here (or on the next line with code), or none\n' +
  'Right-click: only in this node, or in every node';

/** How a row shows the breakpoint on it, by whom it stops. */
const BREAKPOINT_KINDS = ['breakpoint', 'breakpoint-node', 'breakpoint-other'] as const;

/**
 * Where a guest is stopped, if one is: its file always, as with a guest's lines in many, the one
 * on show may not be it.
 */
function stopped(stop: LineStop | null): string {
  if (!stop) return '';
  return `Stopped at line ${stop.line} of ${fileName(stop.file)}${stop.node ? ` in ${stop.node}` : ''}`;
}

/** A file's own name, without its directory. */
function fileName(file: string): string {
  return file.split('/').at(-1) ?? file;
}

/** The end of a path, which is what tells most files apart. */
function shortName(file: string): string {
  return file.split('/').slice(-3).join('/');
}

/**
 * The source of the selected node's guest, to stop on a line of. Its gutter sets breakpoints, and
 * it follows where a guest stops. With no node selected it shows none, and with no guest in the
 * graph that can be stopped on a line, it is hidden.
 */
export class WitgraphSource extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.#sync();
  #picker!: HTMLSelectElement;
  #where!: HTMLSpanElement;
  /** The line of the caller picked in the stack, if one is. */
  #frameLine: number | null = null;
  /** A line to bring into view, once its file is drawn. */
  #scrollTo: number | null = null;
  #scroller!: HTMLDivElement;
  #variables!: HTMLDivElement;
  /** The stop whose variables are on show. */
  #variablesOf: LineStop | null = null;
  /** What each variable held at the stop before, by its path, to mark what has changed. */
  #before = new Map<string, string>();
  /** The function that stop was in. */
  #beforeIn: string | undefined;
  /** The paths left open, so that a step leaves them so. */
  #open = new Set<string>();
  #rows = new Map<number, HTMLDivElement>();
  /** The file on show, and what it was drawn from. */
  #file: string | null = null;
  #drawn: string | null = null;
  #filesShown = '';
  #marksShown = '';
  /** The stop last scrolled to, so a user scrolling away stays where they went. */
  #followed: unknown = null;
  /** The stop, if it is in the guest of the node on show: another's is not marked here. */
  #here: LineStop | null = null;
  /** The node the variables on show are of. */
  #variablesNode: string | undefined;
  /** The node whose files are on show. */
  #node: string | null = null;
  #menu = new ContextMenu();
  #menuLayer!: HTMLDivElement;

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

    this.#scroller = document.createElement('div');
    this.#scroller.className = 'rows';

    this.#variables = document.createElement('div');
    this.#variables.className = 'variables';
    this.#variables.hidden = true;

    const body = document.createElement('div');
    body.className = 'body';
    body.append(this.#scroller, this.#variables);

    this.#menuLayer = document.createElement('div');
    this.#menuLayer.className = 'menu-layer';

    panel.append(header, body);
    shadow.append(panel, this.#menuLayer);
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

  /**
   * Shows the files of the selected node's guest, and none with no node selected. A stop is
   * marked, with its variables, only in its own node's.
   */
  #sync(): void {
    if (!this.#picker) return;
    const controller = this.#controller;
    this.hidden = (controller?.lineFiles.length ?? 0) === 0;
    if (!controller || this.hidden) return;

    const node = controller.selectedNode;
    this.#node = node;
    const files = node === null ? [] : controller.lineFilesOf(node);
    const stop = controller.lineStop;
    const here = stop !== null && node !== null && stop.node === node ? stop : null;
    this.#here = here;
    this.#showVariables(stop);
    this.#variables.hidden ||= this.#variablesNode !== (node ?? undefined);

    this.#picker.hidden = files.length === 0;
    if (files.length === 0) {
      this.#where.textContent = stopped(stop);
      this.#none(
        node === null
          ? 'Select a node to see its source.'
          : `${node} has no source to show: its guest was built without debug info.`,
      );
      return;
    }

    if (here && here !== this.#followed && files.includes(here.file)) this.#file = here.file;
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

    this.#where.textContent = stopped(stop);

    if (this.#drawn !== this.#file) void this.#draw(this.#file);
    else this.#mark();
  }

  /** Shows `message` where a file would be. */
  #none(message: string): void {
    this.#file = null;
    this.#drawn = null;
    this.#rows.clear();
    this.#marksShown = '';
    const none = document.createElement('div');
    none.className = 'missing';
    none.textContent = message;
    this.#scroller.replaceChildren(none);
  }

  /**
   * Shows the variables of a stop, once, and marks what has changed since the one before. Going
   * on from a stop leaves them, dimmed, until the next or until the run is over.
   */
  #showVariables(stop: LineStop | null): void {
    const running = this.#controller?.mode === 'running' || this.#controller?.mode === 'paused';
    this.#variables.classList.toggle('stale', stop === null);
    if (stop === null && !running) {
      // Over: the next run has nothing to differ from.
      this.#variablesOf = null;
      this.#variablesNode = undefined;
      this.#variables.hidden = true;
      this.#before.clear();
      this.#beforeIn = undefined;
      this.#variables.replaceChildren();
      return;
    }
    // Left dimmed from the stop before, while the guest goes on.
    this.#variables.hidden = this.#variables.childElementCount === 0;
    if (stop === this.#variablesOf) return;
    this.#variablesOf = stop;
    if (stop === null) return;
    const { variables } = stop;
    this.#variablesNode = stop.node;
    this.#variables.hidden = false;
    // Another function's variables are others, whatever their names.
    if (stop.frames[0]?.function !== this.#beforeIn) this.#before.clear();
    this.#beforeIn = stop.frames[0]?.function;
    this.#frameLine = null;
    const title = document.createElement('div');
    title.className = 'variables-title';
    title.textContent = 'Variables';
    const now = new Map<string, string>();
    const note = document.createElement('div');
    note.className = 'caller-note';
    note.hidden = true;
    note.textContent = "Of the innermost function: a caller's are out of reach.";
    const shown: HTMLElement[] = [this.#stackRows(stop.frames, note), title, note];
    if (variables.length === 0) {
      const none = document.createElement('div');
      none.className = 'no-variables';
      none.textContent = 'None in scope here.';
      shown.push(none);
    }
    for (const variable of variables) shown.push(this.#variableRow(variable, variable.name, now));
    this.#variables.replaceChildren(...shown);
    this.#before = now;
  }

  /** The stack, innermost first. Choosing a frame shows the line it is at. */
  #stackRows(frames: readonly StackFrame[], note: HTMLElement): HTMLElement {
    const wrap = document.createElement('div');
    wrap.className = 'frames';
    const title = document.createElement('div');
    title.className = 'variables-title';
    title.textContent = 'Call stack';
    wrap.appendChild(title);
    const rows = frames.map((frame, at) => {
      const row = document.createElement('button');
      row.type = 'button';
      row.className = at === 0 ? 'frame selected' : 'frame';
      row.disabled = frame.line === undefined;
      row.title = frame.file ? `${frame.function}\n${frame.file}:${frame.line}` : frame.function;
      const name = document.createElement('span');
      name.className = 'frame-name';
      name.textContent = frame.function;
      row.appendChild(name);
      if (frame.file && frame.line !== undefined) {
        const place = document.createElement('span');
        place.className = 'frame-at';
        place.textContent = `${frame.file.split('/').at(-1)}:${frame.line}`;
        row.appendChild(place);
      }
      row.addEventListener('click', () => {
        for (const other of rows) other.classList.toggle('selected', other === row);
        note.hidden = at === 0;
        // The innermost's line is marked already, as the one it stopped on.
        this.#frameLine = at === 0 ? null : (frame.line ?? null);
        this.#scrollTo = frame.line ?? null;
        if (frame.file) this.#file = frame.file;
        this.#sync();
      });
      return row;
    });
    wrap.append(...rows);
    return wrap;
  }

  #variableRow(variable: SourceVariable, path: string, now: Map<string, string>): HTMLElement {
    now.set(path, variable.value);
    const earlier = this.#before.get(path);
    const changed = this.#before.size > 0 && earlier !== undefined && earlier !== variable.value;

    const line = document.createElement('span');
    line.className = 'variable-line';
    const name = document.createElement('span');
    name.className = 'variable-name';
    name.textContent = `${variable.name}: `;
    const value = document.createElement('span');
    value.className = 'variable-value';
    value.textContent = variable.value;
    const type = document.createElement('span');
    type.className = 'variable-type';
    type.textContent = variable.type;
    line.append(name, value, type);

    if (variable.children.length === 0) {
      const row = document.createElement('div');
      row.className = changed ? 'variable changed' : 'variable';
      row.dataset.path = path;
      row.appendChild(line);
      return row;
    }
    const row = document.createElement('details');
    row.className = changed ? 'variable changed' : 'variable';
    row.dataset.path = path;
    row.open = this.#open.has(path);
    row.addEventListener('toggle', () => {
      if (row.open) this.#open.add(path);
      else this.#open.delete(path);
    });
    const summary = document.createElement('summary');
    summary.appendChild(line);
    row.appendChild(summary);
    for (const child of variable.children) {
      row.appendChild(this.#variableRow(child, `${path}.${child.name}`, now));
    }
    return row;
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
      gutter.title = GUTTER_TITLE;
      const dot = document.createElement('span');
      dot.className = 'dot';
      gutter.appendChild(dot);
      gutter.addEventListener('click', () => controller.toggleLineBreakpoint(file, number));
      gutter.addEventListener('contextmenu', (event) => {
        event.preventDefault();
        this.#breakpointMenu(event, file, number);
      });
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

  /**
   * Offers what can be done with the breakpoint on a line: set it, take it away, or have it stop
   * only the node on show, or every node.
   */
  #breakpointMenu(event: MouseEvent, file: string, line: number): void {
    const controller = this.#controller;
    const node = this.#node;
    if (!controller || node === null) return;
    const set = controller.lineBreakpointAt(file, line)?.scope;
    const to = (scope: BreakpointScope | null) => () =>
      controller.setLineBreakpoint(file, line, scope);
    const only = { label: `Only in ${node}`, action: to({ node }) };
    const every = { label: 'In every node', action: to('every-node') };
    const remove = (label: string) => ({ label, shortcut: 'Click', action: to(null) });
    let items: MenuItem[];
    if (set === undefined) {
      items = [
        { label: 'Add breakpoint', shortcut: 'Click', action: to('every-node') },
        { label: `Add breakpoint only in ${node}`, action: to({ node }) },
      ];
    } else if (set === 'every-node') {
      items = [remove('Remove breakpoint'), only];
    } else if (set.node === node) {
      items = [remove('Remove breakpoint'), every];
    } else {
      items = [remove(`Remove breakpoint (only in ${set.node})`), only, every];
    }
    this.#menu.show(event.clientX, event.clientY, items, this.#menuLayer);
  }

  /** Marks the breakpoints and the current line, if they moved. */
  #mark(): void {
    const controller = this.#controller;
    const file = this.#file;
    if (!controller || file === null) return;
    const stop = this.#here;
    const current = stop?.file === file ? stop.line : null;
    const node = this.#node;
    const kinds = new Map<number, { kind: string; title: string }>();
    for (const { scope, line } of controller.lineBreakpoints(file)) {
      if (scope === 'every-node') {
        kinds.set(line, { kind: 'breakpoint', title: 'Breakpoint, in every node' });
      } else if (scope.node === node) {
        kinds.set(line, { kind: 'breakpoint-node', title: `Breakpoint, only in ${node}` });
      } else {
        kinds.set(line, {
          kind: 'breakpoint-other',
          title: `Breakpoint, only in ${scope.node}: ${node} runs past it`,
        });
      }
    }
    const marks = `${current}|${this.#frameLine}|${node}|${JSON.stringify([...kinds])}`;
    if (marks !== this.#marksShown) {
      this.#marksShown = marks;
      for (const [number, row] of this.#rows) {
        const set = kinds.get(number);
        for (const kind of BREAKPOINT_KINDS) row.classList.toggle(kind, set?.kind === kind);
        const gutter = row.querySelector<HTMLElement>('.gutter');
        if (gutter) gutter.title = set ? `${set.title}\n${GUTTER_TITLE}` : GUTTER_TITLE;
        row.classList.toggle('current', number === current);
        row.classList.toggle('frame-line', number === this.#frameLine);
      }
    }
    if (stop && stop !== this.#followed && current !== null) {
      this.#followed = stop;
      this.#scrollTo = current;
    }
    const row = this.#scrollTo === null ? undefined : this.#rows.get(this.#scrollTo);
    if (row) {
      this.#scrollTo = null;
      const from = this.#scroller.getBoundingClientRect();
      const at = row.getBoundingClientRect();
      this.#scroller.scrollTop += at.top - from.top - from.height / 2;
    }
  }
}
