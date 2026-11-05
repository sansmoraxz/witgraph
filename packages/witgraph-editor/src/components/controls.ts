import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Camera,
  Circle,
  createElement,
  Download,
  FastForward,
  History,
  LayoutGrid,
  Package,
  Pause,
  Play,
  Redo2,
  RedoDot,
  RotateCcw,
  RotateCw,
  Square,
  StepForward,
  Undo2,
  Upload,
} from 'lucide';
import type { EditorController } from '../controller/editor-controller.ts';
import type { EditorMode } from '../controller/types.ts';
import { adoptStyles } from './shared-styles.ts';

const MODE_LABELS: Record<EditorMode, string> = {
  idle: 'Idle',
  running: 'Running…',
  paused: 'Paused',
  error: 'Error',
};

const MODE_BG: Record<EditorMode, string> = {
  idle: 'bg-wg-phase-idle',
  running: 'bg-wg-phase-running',
  paused: 'bg-wg-warning',
  error: 'bg-wg-phase-faulted',
};

const EXTRA_CSS = `
  button[hidden], .btn-group[hidden], .sep[hidden], .level[hidden] { display: none; }
  .level { font-size: 0.6875rem; color: var(--wg-text-dim); margin: 0 2px 0 4px; }
  :host { display: block;
    --_btn-radius: var(--wg-btn-radius, 4px);
    --_group-radius: var(--wg-group-radius, 6px); }
  .toolbar { flex-wrap: wrap; display: flex; align-items: center; gap: 4px; padding: 6px 10px;
    background: var(--wg-surface); border-bottom: 1px solid var(--wg-bg); }
  .btn-group { display: inline-flex; gap: 2px; background: color-mix(in srgb, var(--wg-bg) 50%, transparent);
    padding: 2px; border-radius: var(--_group-radius); }
  .btn { display: inline-flex; align-items: center; gap: 4px;
    background: transparent; color: var(--wg-text); border: none; border-radius: var(--_btn-radius);
    padding: 5px 10px; cursor: pointer; font-family: system-ui, sans-serif;
    font-size: 0.6875rem; font-weight: 500; white-space: nowrap;
    transition: all 0.12s ease; user-select: none; }
  .btn svg { width: 14px; height: 14px; stroke-width: 2; }
  .btn:hover:not(:disabled) { background: var(--wg-surface-hover); }
  .btn:active:not(:disabled) { background: var(--wg-overlay); transform: scale(0.97); }
  .btn:disabled { opacity: 0.3; cursor: not-allowed; }
  .btn-primary { background: var(--wg-primary); color: var(--wg-bg); font-weight: 600; }
  .btn-primary:hover:not(:disabled) { background: var(--wg-primary-hover); }
  .btn-primary svg { stroke: var(--wg-bg); }
  .btn-danger { color: var(--wg-error); }
  .btn-danger:hover:not(:disabled) { background: color-mix(in srgb, var(--wg-error) 15%, transparent); }
  .btn-danger svg { stroke: var(--wg-error); }
  .btn-warn { color: var(--wg-warning); }
  .btn-warn:hover:not(:disabled) { background: color-mix(in srgb, var(--wg-warning) 15%, transparent); }
  .btn-warn svg { stroke: var(--wg-warning); }
  .sep { width: 1px; height: 24px; background: var(--wg-surface-hover); margin: 0 4px; flex-shrink: 0; }
  .badge { padding: 4px 10px; border-radius: var(--_group-radius); font-size: 0.6875rem;
    font-weight: 700; color: var(--wg-bg); white-space: nowrap; }
  .duration { font-weight: 400; opacity: 0.7; margin-left: 4px; font-size: 0.625rem; }
  .spacer { flex: 1; }
  .icon-only svg { width: 16px; height: 16px; }
  .icon-only { padding: 5px 7px; }
  .rate-group { display: inline-flex; align-items: center; gap: 4px;
    background: color-mix(in srgb, var(--wg-bg) 50%, transparent); padding: 2px 8px; border-radius: var(--_group-radius); }
  .rate-label { color: var(--wg-text-dim); font-size: 0.625rem; font-weight: 500;
    white-space: nowrap; user-select: none; }
  .rate-slider { width: 80px; height: 4px; accent-color: var(--wg-primary); cursor: pointer; }
  .rate-value { color: var(--wg-text); font-size: 0.625rem; font-weight: 600;
    min-width: 32px; text-align: right; font-variant-numeric: tabular-nums; }
`;

/** The rates the tick rate slider steps through, in ticks a second; `0` is no limit. */
const TICK_RATES = [1, 2, 5, 10, 20, 30, 60, 0];

const ICONS: Record<string, unknown[]> = {
  play: Play,
  pause: Pause,
  'rotate-cw': RotateCw,
  'redo-dot': RedoDot,
  'arrow-down-to-line': ArrowDownToLine,
  'arrow-up-from-line': ArrowUpFromLine,
  'step-forward': StepForward,
  square: Square,
  'rotate-ccw': RotateCcw,
  circle: Circle,
  'fast-forward': FastForward,
  'layout-grid': LayoutGrid,
  'undo-2': Undo2,
  'redo-2': Redo2,
  upload: Upload,
  download: Download,
  package: Package,
  camera: Camera,
  history: History,
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
  #btnPause!: HTMLButtonElement;
  #btnStepOver!: HTMLButtonElement;
  #btnStepIn!: HTMLButtonElement;
  #levelCode!: HTMLSpanElement;
  #sepCode!: HTMLDivElement;
  #btnStepOut!: HTMLButtonElement;
  #btnRestart!: HTMLButtonElement;
  #btnStop!: HTMLButtonElement;
  #btnReset!: HTMLButtonElement;
  #btnUndo!: HTMLButtonElement;
  #btnRedo!: HTMLButtonElement;
  #btnBreakpoint!: HTMLButtonElement;
  #btnResume!: HTMLButtonElement;
  #btnLayout!: HTMLButtonElement;
  #btnSnapSave!: HTMLButtonElement;
  #btnSnapRestore!: HTMLButtonElement;
  #btnLoad!: HTMLButtonElement;
  #badge!: HTMLSpanElement;
  /** Shows a tick rate on the slider; set once the toolbar is built. */
  #showRate: ((rate: number) => void) | null = null;
  #durationSpan!: HTMLSpanElement;

  /** The usual debugger keys, wherever the focus is. */
  #onKeyDown = (event: KeyboardEvent) => {
    const controller = this.#controller;
    if (!controller) return;
    const act = (action: () => unknown) => {
      event.preventDefault();
      void action();
    };
    const { key, shiftKey, ctrlKey, metaKey } = event;
    if (key === 'F5' && shiftKey && (ctrlKey || metaKey)) act(() => controller.restart());
    else if (key === 'F5' && shiftKey) act(() => controller.stop());
    else if (key === 'F5' && !ctrlKey && !metaKey) act(() => controller.run());
    else if (key === 'F6') act(() => controller.pause());
    else if (key === 'F10') {
      // What is next, at the level it is stopped at.
      act(() => (controller.lineStop ? controller.stepOver() : controller.step()));
    } else if (key === 'F11' && shiftKey) act(() => controller.stepOut());
    else if (key === 'F11') act(() => controller.stepIn());
    else if (key === 'F9' && controller.selectedNode) {
      const node = controller.selectedNode;
      act(() => controller.toggleBreakpoint(node));
    }
  };

  connectedCallback(): void {
    document.addEventListener('keydown', this.#onKeyDown);
    if (this.shadowRoot) return;
    const shadow = this.attachShadow({ mode: 'open' });
    adoptStyles(shadow, EXTRA_CSS);

    const toolbar = document.createElement('div');
    toolbar.className = 'toolbar';

    const mkBtn = (
      cls: string,
      title: string,
      iconName: string,
      label?: string,
    ): HTMLButtonElement => {
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

    // Running: the same whatever it is stopped at.
    this.#btnRun = mkBtn('run btn-primary', 'Run to the next breakpoint (F5)', 'play', 'Run');
    this.#btnResume = mkBtn(
      'resume',
      'Continue to the next breakpoint, of a node or a line (F5)',
      'fast-forward',
      'Continue',
    );
    this.#btnPause = mkBtn(
      'pause',
      'Pause before the next node, or at the next line with debugging on (F6)',
      'pause',
      'Pause',
    );
    this.#btnStop = mkBtn('stop btn-danger', 'Stop (Shift+F5)', 'square', 'Stop');
    this.#btnRestart = mkBtn(
      'restart',
      'Run again from the top (Ctrl+Shift+F5)',
      'rotate-cw',
      'Restart',
    );
    this.#btnReset = mkBtn('reset', 'Reset', 'rotate-ccw', 'Reset');
    group(
      this.#btnRun,
      this.#btnResume,
      this.#btnPause,
      this.#btnStop,
      this.#btnRestart,
      this.#btnReset,
    );

    sep();

    // Debugging the graph: whole nodes.
    const level = (text: string) => {
      const label = document.createElement('span');
      label.className = 'level';
      label.textContent = text;
      toolbar.appendChild(label);
      return label;
    };
    level('Graph');
    this.#btnStep = mkBtn(
      'step',
      'Finish this node and pause before the next (F10 unless stopped on a line)',
      'step-forward',
      'Next node',
    );
    this.#btnBreakpoint = mkBtn(
      'breakpoint btn-warn',
      'Pause before the selected node runs (F9)',
      'circle',
      'Break on node',
    );
    group(this.#btnStep, this.#btnBreakpoint);

    // Debugging a guest's source: lines. Only with debugging on, and debug info to go by.
    this.#sepCode = document.createElement('div');
    this.#sepCode.className = 'sep';
    toolbar.appendChild(this.#sepCode);
    this.#levelCode = level('Code');
    this.#btnStepOver = mkBtn(
      'step-over',
      'Go to the next line, over any call on this one (F10)',
      'redo-dot',
      'Over',
    );
    this.#btnStepIn = mkBtn(
      'step-in',
      'Go into the call on this line, or into the code of the node about to run (F11)',
      'arrow-down-to-line',
      'Into',
    );
    this.#btnStepOut = mkBtn(
      'step-out',
      'Run until this function returns (Shift+F11)',
      'arrow-up-from-line',
      'Out',
    );
    group(this.#btnStepOver, this.#btnStepIn, this.#btnStepOut);

    sep();

    // Edit group
    this.#btnUndo = mkBtn('undo icon-only', 'Undo (Ctrl+Z)', 'undo-2');
    this.#btnRedo = mkBtn('redo icon-only', 'Redo (Ctrl+Y)', 'redo-2');
    this.#btnLayout = mkBtn('layout', 'Auto Layout', 'layout-grid', 'Layout');
    group(this.#btnUndo, this.#btnRedo, this.#btnLayout);

    sep();

    // Simulation group
    this.#btnSnapSave = mkBtn('snap-save', 'Save snapshot', 'camera', 'Save');
    this.#btnSnapRestore = mkBtn('snap-restore', 'Restore last snapshot', 'history', 'Restore');
    group(this.#btnSnapSave, this.#btnSnapRestore);

    sep();

    // Load
    this.#btnLoad = mkBtn('load', 'Load graph from JSON file', 'upload', 'Load');
    group(this.#btnLoad);
    this.#btnLoad.addEventListener('click', () => this.#controller?.importGraphFile());

    sep();

    // Export
    const btnDownload = mkBtn('download', 'Download graph JSON', 'download', 'JSON');
    const btnBundle = mkBtn('bundle', 'Download bundle (graph + components)', 'package', 'Bundle');
    group(btnDownload, btnBundle);
    btnDownload.addEventListener('click', () => this.#controller?.downloadGraph());
    btnBundle.addEventListener('click', () => this.#controller?.downloadBundle());

    sep();

    // Tick rate slider: a step per rate, slowest first, `Max` last
    const rateGroup = document.createElement('div');
    rateGroup.className = 'rate-group';
    const rateLabel = document.createElement('span');
    rateLabel.className = 'rate-label';
    rateLabel.textContent = 'Rate';
    const rateSlider = document.createElement('input');
    rateSlider.type = 'range';
    rateSlider.className = 'rate-slider';
    rateSlider.min = '0';
    rateSlider.max = String(TICK_RATES.length - 1);
    rateSlider.step = '1';
    rateSlider.title = 'Most ticks a second a run makes';
    const rateValue = document.createElement('span');
    rateValue.className = 'rate-value';
    const showRate = (rate: number) => {
      const index = TICK_RATES.indexOf(rate);
      rateSlider.value = String(index < 0 ? TICK_RATES.length - 1 : index);
      rateValue.textContent = rate > 0 ? `${rate}/s` : 'Max';
    };
    showRate(this.#controller?.tickRate ?? 0);
    rateSlider.addEventListener('input', () => {
      const rate = TICK_RATES[parseInt(rateSlider.value, 10)] ?? 0;
      showRate(rate);
      if (this.#controller) this.#controller.tickRate = rate;
    });
    this.#showRate = showRate;
    rateGroup.append(rateLabel, rateSlider, rateValue);
    toolbar.appendChild(rateGroup);

    // Spacer + badge
    const spacer = document.createElement('div');
    spacer.className = 'spacer';
    toolbar.appendChild(spacer);

    this.#badge = document.createElement('span');
    this.#badge.className = 'badge';
    this.#badge.appendChild(document.createTextNode(''));
    this.#durationSpan = document.createElement('span');
    this.#durationSpan.className = 'duration';
    this.#badge.appendChild(this.#durationSpan);
    toolbar.appendChild(this.#badge);

    shadow.appendChild(toolbar);

    this.#btnRun.addEventListener('click', () => this.#controller?.run());
    this.#btnStep.addEventListener('click', () => this.#controller?.step());
    this.#btnStepOver.addEventListener('click', () => this.#controller?.stepOver());
    this.#btnStepIn.addEventListener('click', () => this.#controller?.stepIn());
    this.#btnStepOut.addEventListener('click', () => this.#controller?.stepOut());
    this.#btnPause.addEventListener('click', () => this.#controller?.pause());
    this.#btnRestart.addEventListener('click', () => this.#controller?.restart());
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
    this.#btnSnapSave.addEventListener('click', () => {
      try {
        void this.#controller?.saveSnapshot();
      } catch {}
    });
    this.#btnSnapRestore.addEventListener('click', () => {
      const snaps = this.#controller?.savedSnapshots ?? [];
      if (snaps.length > 0) {
        void this.#controller?.restoreNamedSnapshot(snaps[snaps.length - 1].name).catch(() => {});
      }
    });

    this.#render();
  }

  disconnectedCallback(): void {
    document.removeEventListener('keydown', this.#onKeyDown);
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
  }

  set controller(ctrl: EditorController) {
    this.#controller?.removeEventListener('state-changed', this.#onStateChanged);
    this.#controller = ctrl;
    ctrl.addEventListener('state-changed', this.#onStateChanged);
    this.#showRate?.(ctrl.tickRate);
    this.#render();
  }

  get controller(): EditorController | null {
    return this.#controller;
  }

  #render(): void {
    if (!this.#btnRun) return;
    const mode = this.#controller?.mode ?? 'idle';
    const hasSelection = !!this.#controller?.selectedNode;

    // Paused, Continue is what goes on, and Next node goes on by one node.
    this.#btnRun.disabled = mode === 'running' || mode === 'paused';
    this.#btnStep.disabled = mode === 'running';
    // Only with code to step through: most graphs have no debug info.
    const hasLines = (this.#controller?.lineFiles.length ?? 0) > 0;
    const onLine = this.#controller?.lineStop != null;
    this.#btnStepOver.parentElement?.toggleAttribute('hidden', !hasLines);
    this.#levelCode.hidden = !hasLines;
    this.#sepCode.hidden = !hasLines;
    this.#btnStepOver.disabled = !onLine;
    this.#btnStepIn.disabled = mode !== 'paused';
    this.#btnStepOut.disabled = !onLine;
    this.#btnPause.disabled = mode !== 'running';
    this.#btnRestart.disabled = !this.#controller;
    // A step can leave a stream going: stopping is for that too.
    const anyRunning = [...(this.#controller?.nodes.values() ?? [])].some(
      (node) => node.phase === 'running',
    );
    this.#btnStop.disabled = !anyRunning && (mode === 'idle' || mode === 'error');
    // What changes the graph waits while it runs.
    const editable = this.#controller?.editable ?? true;
    this.#btnReset.disabled = !editable;
    this.#btnLoad.disabled = !editable;
    this.#btnBreakpoint.disabled = !hasSelection;
    this.#btnResume.disabled = mode !== 'paused';
    this.#btnLayout.disabled = mode === 'running';
    this.#btnUndo.disabled = !editable || !(this.#controller?.canUndo ?? false);
    this.#btnRedo.disabled = !editable || !(this.#controller?.canRedo ?? false);
    this.#btnSnapSave.disabled = !this.#controller || mode === 'running';
    this.#btnSnapRestore.disabled =
      !this.#controller || this.#controller.savedSnapshots.length === 0 || mode === 'running';
    // An attached graph is the program's to run, stop and change: here it is only debugged.
    const attached = this.#controller?.attached ?? false;
    for (const button of [this.#btnRun, this.#btnStop, this.#btnRestart, this.#btnReset]) {
      button.toggleAttribute('hidden', attached);
    }
    for (const button of [this.#btnLoad, this.#btnSnapSave, this.#btnSnapRestore]) {
      if (attached) button.disabled = true;
    }

    // Between ticks too: a step can leave a stream going.
    const shown: EditorMode = mode === 'idle' && anyRunning ? 'running' : mode;
    let badgeText = MODE_LABELS[shown];
    if (mode === 'paused') badgeText = onLine ? 'Paused on a line' : 'Paused at a node';
    if (attached && mode === 'running') badgeText = 'Attached';
    if (mode === 'idle' && this.#controller?.lastRunDurationMs != null) {
      const ms = this.#controller.lastRunDurationMs;
      const ticks = this.#controller.lastRunTicks;
      const timeStr = ms < 1000 ? `${ms.toFixed(0)}ms` : `${(ms / 1000).toFixed(2)}s`;
      badgeText = shown === 'running' ? 'Running' : 'Idle';
      this.#durationSpan.textContent = ` ${ticks} tick${ticks !== 1 ? 's' : ''} · ${timeStr}`;
    } else {
      this.#durationSpan.textContent = '';
    }
    this.#badge.childNodes[0].textContent = badgeText;
    this.#badge.className = `badge ${MODE_BG[shown]}`;
  }
}

if (!customElements.get('witgraph-controls'))
  customElements.define('witgraph-controls', WitgraphControls);
