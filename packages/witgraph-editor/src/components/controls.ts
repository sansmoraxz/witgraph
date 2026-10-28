import { adoptStyles } from './shared-styles.ts';
import type { EditorController } from '../controller/editor-controller.ts';
import type { EditorMode } from '../controller/types.ts';
import { createElement } from 'lucide';
import { Play, StepForward, Square, RotateCcw, Circle, FastForward, LayoutGrid, Undo2, Redo2, Upload, Download, Package } from 'lucide';

const MODE_LABELS: Record<EditorMode, string> = {
  idle: 'Idle', running: 'Running…', paused: 'Paused', error: 'Error',
};

const MODE_BG: Record<EditorMode, string> = {
  idle: 'bg-wg-phase-idle', running: 'bg-wg-phase-running',
  paused: 'bg-wg-phase-pending', error: 'bg-wg-phase-faulted',
};

const EXTRA_CSS = `
  :host { display: block; }
  .toolbar { display: flex; align-items: center; gap: 4px; padding: 6px 10px;
    background: #313244; border-bottom: 1px solid #1e1e2e; }
  .btn-group { display: inline-flex; gap: 2px; background: rgba(30,30,46,0.5);
    padding: 2px; border-radius: 6px; }
  .btn { display: inline-flex; align-items: center; gap: 4px;
    background: transparent; color: #cdd6f4; border: none; border-radius: 4px;
    padding: 5px 10px; cursor: pointer; font-family: system-ui, sans-serif;
    font-size: 0.6875rem; font-weight: 500; white-space: nowrap;
    transition: all 0.12s ease; user-select: none; }
  .btn svg { width: 14px; height: 14px; stroke-width: 2; }
  .btn:hover:not(:disabled) { background: #45475a; }
  .btn:active:not(:disabled) { background: #585b70; transform: scale(0.97); }
  .btn:disabled { opacity: 0.3; cursor: not-allowed; }
  .btn-primary { background: #89b4fa; color: #1e1e2e; font-weight: 600; }
  .btn-primary:hover:not(:disabled) { background: #74c7ec; }
  .btn-primary svg { stroke: #1e1e2e; }
  .btn-danger { color: #f38ba8; }
  .btn-danger:hover:not(:disabled) { background: rgba(243,139,168,0.15); }
  .btn-danger svg { stroke: #f38ba8; }
  .btn-warn { color: #fab387; }
  .btn-warn:hover:not(:disabled) { background: rgba(250,179,135,0.15); }
  .btn-warn svg { stroke: #fab387; }
  .sep { width: 1px; height: 24px; background: #45475a; margin: 0 4px; flex-shrink: 0; }
  .badge { padding: 4px 10px; border-radius: 6px; font-size: 0.6875rem;
    font-weight: 700; color: #1e1e2e; white-space: nowrap; }
  .duration { font-weight: 400; opacity: 0.7; margin-left: 4px; font-size: 0.625rem; }
  .spacer { flex: 1; }
  .icon-only svg { width: 16px; height: 16px; }
  .icon-only { padding: 5px 7px; }
  .delay-group { display: inline-flex; align-items: center; gap: 4px;
    background: rgba(30,30,46,0.5); padding: 2px 8px; border-radius: 6px; }
  .delay-label { color: #a6adc8; font-size: 0.625rem; font-weight: 500;
    white-space: nowrap; user-select: none; }
  .delay-slider { width: 80px; height: 4px; accent-color: #89b4fa; cursor: pointer; }
  .delay-value { color: #cdd6f4; font-size: 0.625rem; font-weight: 600;
    min-width: 32px; text-align: right; font-variant-numeric: tabular-nums; }
`;

const ICONS: Record<string, unknown[]> = {
  play: Play, 'step-forward': StepForward, square: Square, 'rotate-ccw': RotateCcw,
  circle: Circle, 'fast-forward': FastForward, 'layout-grid': LayoutGrid,
  'undo-2': Undo2, 'redo-2': Redo2, upload: Upload, download: Download, package: Package,
};

function iconEl(name: string): SVGSVGElement | null {
  const def = ICONS[name];
  if (!def) return null;
  return createElement(def as Parameters<typeof createElement>[0]) as unknown as SVGSVGElement;
}

export class WitgraphControls extends HTMLElement {
  #controller: EditorController | null = null;
  #onStateChanged = () => this.#render();
  #btnRun!: HTMLButtonElement;
  #btnStep!: HTMLButtonElement;
  #btnStop!: HTMLButtonElement;
  #btnReset!: HTMLButtonElement;
  #btnUndo!: HTMLButtonElement;
  #btnRedo!: HTMLButtonElement;
  #btnBreakpoint!: HTMLButtonElement;
  #btnResume!: HTMLButtonElement;
  #btnLayout!: HTMLButtonElement;
  #badge!: HTMLSpanElement;
  #durationSpan!: HTMLSpanElement;

  connectedCallback(): void {
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);

    const toolbar = document.createElement('div');
    toolbar.className = 'toolbar';

    const mkBtn = (cls: string, title: string, iconName: string, label?: string): HTMLButtonElement => {
      const b = document.createElement('button');
      b.className = `btn ${cls}`;
      b.title = title;
      const svg = iconEl(iconName);
      if (svg) b.appendChild(svg);
      if (label) b.appendChild(document.createTextNode(` ${label}`));
      if (!label) b.classList.add('icon-only');
      return b;
    };

    const group = (...btns: HTMLButtonElement[]) => {
      const g = document.createElement('div');
      g.className = 'btn-group';
      for (const b of btns) g.appendChild(b);
      toolbar.appendChild(g);
    };

    const sep = () => {
      const s = document.createElement('div');
      s.className = 'sep';
      toolbar.appendChild(s);
    };

    // Execution group
    this.#btnRun = mkBtn('run btn-primary', 'Run (settle)', 'play', 'Run');
    this.#btnStep = mkBtn('step', 'Step (single tick)', 'step-forward', 'Step');
    this.#btnStop = mkBtn('stop btn-danger', 'Stop', 'square', 'Stop');
    this.#btnReset = mkBtn('reset', 'Reset', 'rotate-ccw', 'Reset');
    group(this.#btnRun, this.#btnStep, this.#btnStop, this.#btnReset);

    sep();

    // Debug group
    this.#btnBreakpoint = mkBtn('breakpoint btn-warn', 'Toggle Breakpoint', 'circle', 'BP');
    this.#btnResume = mkBtn('resume', 'Resume from breakpoint', 'fast-forward', 'Resume');
    group(this.#btnBreakpoint, this.#btnResume);

    sep();

    // Edit group
    this.#btnUndo = mkBtn('undo icon-only', 'Undo (Ctrl+Z)', 'undo-2');
    this.#btnRedo = mkBtn('redo icon-only', 'Redo (Ctrl+Y)', 'redo-2');
    this.#btnLayout = mkBtn('layout', 'Auto Layout', 'layout-grid', 'Layout');
    group(this.#btnUndo, this.#btnRedo, this.#btnLayout);

    sep();

    // Load
    const btnLoad = mkBtn('load', 'Load graph from JSON file', 'upload', 'Load');
    group(btnLoad);
    btnLoad.addEventListener('click', () => this.#controller?.importGraphFile());

    sep();

    // Export
    const btnDownload = mkBtn('download', 'Download graph JSON', 'download', 'JSON');
    const btnBundle = mkBtn('bundle', 'Download bundle (graph + components)', 'package', 'Bundle');
    group(btnDownload, btnBundle);
    btnDownload.addEventListener('click', () => this.#controller?.downloadGraph());
    btnBundle.addEventListener('click', () => this.#controller?.downloadBundle());

    sep();

    // Tick delay slider
    const delayGroup = document.createElement('div');
    delayGroup.className = 'delay-group';
    const delayLabel = document.createElement('span');
    delayLabel.className = 'delay-label';
    delayLabel.textContent = 'Delay';
    const delaySlider = document.createElement('input');
    delaySlider.type = 'range';
    delaySlider.className = 'delay-slider';
    delaySlider.min = '0';
    delaySlider.max = '2000';
    delaySlider.step = '50';
    delaySlider.value = '0';
    delaySlider.title = 'Artificial delay between ticks (ms)';
    const delayValue = document.createElement('span');
    delayValue.className = 'delay-value';
    delayValue.textContent = '0ms';
    delaySlider.addEventListener('input', () => {
      const ms = parseInt(delaySlider.value, 10);
      delayValue.textContent = `${ms}ms`;
      if (this.#controller) this.#controller.tickDelayMs = ms;
    });
    delayGroup.append(delayLabel, delaySlider, delayValue);
    toolbar.appendChild(delayGroup);

    // Spacer + badge
    const spacer = document.createElement('div');
    spacer.className = 'spacer';
    toolbar.appendChild(spacer);

    this.#badge = document.createElement('span');
    this.#badge.className = 'badge';
    this.#durationSpan = document.createElement('span');
    this.#durationSpan.className = 'duration';
    this.#badge.appendChild(this.#durationSpan);
    toolbar.appendChild(this.#badge);

    shadow.appendChild(toolbar);

    this.#btnRun.addEventListener('click', () => this.#controller?.run());
    this.#btnStep.addEventListener('click', () => this.#controller?.step());
    this.#btnStop.addEventListener('click', () => this.#controller?.stop());
    this.#btnReset.addEventListener('click', () => this.#controller?.reset());
    this.#btnUndo.addEventListener('click', () => this.#controller?.undo());
    this.#btnRedo.addEventListener('click', () => this.#controller?.redo());
    this.#btnBreakpoint.addEventListener('click', () => {
      const sel = this.#controller?.selectedNode;
      if (sel) this.#controller!.toggleBreakpoint(sel);
    });
    this.#btnResume.addEventListener('click', () => this.#controller?.resumeBreakpoint());
    this.#btnLayout.addEventListener('click', () => this.#controller?.autoLayout());

    this.#render();
  }

  disconnectedCallback(): void {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  set controller(ctrl: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.#render();
  }

  get controller(): EditorController | null { return this.#controller; }

  #render(): void {
    if (!this.#btnRun) return;
    const mode = this.#controller?.mode ?? 'idle';
    const hasSelection = !!this.#controller?.selectedNode;

    this.#btnRun.disabled = mode === 'running';
    this.#btnStep.disabled = mode === 'running';
    this.#btnStop.disabled = mode === 'idle' || mode === 'error';
    this.#btnReset.disabled = false;
    this.#btnBreakpoint.disabled = !hasSelection;
    this.#btnResume.disabled = mode !== 'paused';
    this.#btnLayout.disabled = mode === 'running';
    this.#btnUndo.disabled = !(this.#controller?.canUndo ?? false);
    this.#btnRedo.disabled = !(this.#controller?.canRedo ?? false);

    let badgeText = MODE_LABELS[mode];
    if (mode === 'idle' && this.#controller?.lastRunDurationMs != null) {
      const ms = this.#controller.lastRunDurationMs;
      const ticks = this.#controller.lastRunTicks;
      const timeStr = ms < 1000 ? `${ms.toFixed(0)}ms` : `${(ms / 1000).toFixed(2)}s`;
      badgeText = `Idle`;
      this.#durationSpan.textContent = ` ${ticks} tick${ticks !== 1 ? 's' : ''} · ${timeStr}`;
    } else {
      this.#durationSpan.textContent = '';
    }
    this.#badge.childNodes[0].textContent = badgeText;
    this.#badge.className = `badge ${MODE_BG[mode]}`;
  }
}

customElements.define('witgraph-controls', WitgraphControls);
