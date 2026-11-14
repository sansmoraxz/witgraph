/**
 * What the inspector shows of a port that changes while it is shown: an output's value, and
 * the field injecting an input. Each is an element of its own that follows the controller
 * itself, in a shadow root of its own, so the inspector around it never draws it again for
 * a new value, and a value being typed is never lost to a redraw.
 */

import type { EditorController } from '../controller/editor-controller.ts';
import { outputElement, outputText } from '../util/output-text.ts';
import { READABLE_STYLES } from '../util/readable.ts';
import { adoptStyles } from './shared-styles.ts';

export const BTN =
  'bg-wg-surface text-wg-text border border-wg-text-dim rounded px-2 py-1 cursor-pointer text-xs hover:bg-wg-surface-hover transition-all shadow-sm';

/** An element of a port of a node, following a controller while it is connected. */
abstract class PortField extends HTMLElement {
  #controller: EditorController | null = null;
  #onChange = () => this.update();

  constructor(css: string) {
    super();
    adoptStyles(this.attachShadow({ mode: 'open' }), css);
  }

  /** The node and its port, set once, before it is connected. */
  node = '';
  port = '';

  set controller(controller: EditorController) {
    this.#unfollow();
    this.#controller = controller;
    if (this.isConnected) this.#follow();
  }

  get controller(): EditorController | null {
    return this.#controller;
  }

  connectedCallback(): void {
    this.#follow();
  }

  disconnectedCallback(): void {
    this.#unfollow();
  }

  #follow(): void {
    const controller = this.#controller;
    if (!controller) return;
    for (const event of this.events) controller.addEventListener(event, this.#onChange);
    this.update();
  }

  #unfollow(): void {
    for (const event of this.events) this.#controller?.removeEventListener(event, this.#onChange);
  }

  /** The controller's events that may change what it shows. */
  protected abstract readonly events: readonly string[];

  /** Shows what is current, touching nothing that has not changed. */
  protected abstract update(): void;
}

/** An output's latest value, as people read it, shown afresh as it changes. */
export class WitgraphOutputValue extends PortField {
  protected readonly events = ['output-changed', 'state-changed'];
  /** What is shown, on one line: what tells a change. */
  #shown: string | undefined;

  constructor() {
    super(`:host { display: block; } ${READABLE_STYLES}`);
  }

  protected update(): void {
    const controller = this.controller;
    const root = this.shadowRoot;
    if (!controller || !root) return;
    let text: string;
    let shown: Node;
    try {
      text = outputText(controller, this.node, this.port);
      shown = outputElement(controller, this.node, this.port);
    } catch {
      text = '-';
      shown = document.createTextNode('-');
    }
    if (text === this.#shown) return;
    this.#shown = text;
    root.replaceChildren(shown);
  }
}

/**
 * The field injecting an input a value, as WAVE text: what is injected, and the box to type
 * another in. What is typed stays as it is while what is injected changes.
 */
export class WitgraphInjectField extends PortField {
  protected readonly events = ['state-changed'];
  #injected = document.createElement('div');
  #input = document.createElement('input');
  #error = document.createElement('div');
  /** What is injected, as last shown: `null` before the first. */
  #shown: string | undefined | null = null;

  constructor() {
    super(`:host { display: block; } .hidden { display: none; }
      input[type="text"] { background-color: var(--wg-bg); color: var(--wg-text);
        border: 1px solid var(--wg-surface-hover); border-radius: var(--_input-radius, 4px);
        padding: 4px 6px; font-size: 11px; }
      input:focus { border-color: var(--wg-primary); outline: none; }
      .focus-ring:focus { border-color: var(--wg-primary); outline: none;
        box-shadow: 0 0 0 2px color-mix(in srgb, var(--wg-primary) 25%, transparent); }`);
    const root = this.shadowRoot as ShadowRoot;
    this.#injected.className = 'flex items-center gap-1.5 mb-1 hidden';
    const form = document.createElement('div');
    form.className = 'flex gap-1.5 items-center';
    const input = this.#input;
    input.type = 'text';
    input.className =
      'focus-ring flex-1 bg-wg-bg text-wg-text border border-wg-text-dim rounded px-2 py-1 font-mono text-xs';
    input.style.minWidth = '0';
    input.placeholder = 'WAVE value…';
    const inject = document.createElement('button');
    inject.className = BTN;
    inject.textContent = 'Inject';
    const clear = document.createElement('button');
    clear.className = `${BTN} opacity-85`;
    clear.textContent = '×';
    clear.title = 'Clear';
    form.append(input, inject, clear);
    this.#error.className = 'text-wg-error text-xs mt-1 p-1 bg-wg-error-10 rounded hidden';
    root.append(this.#injected, form, this.#error);

    const submit = async () => {
      this.#showError(undefined);
      try {
        await this.controller?.injectValue(this.node, this.port, input.value);
      } catch (error: unknown) {
        this.#showError(error instanceof Error ? error.message : String(error));
      }
    };
    inject.addEventListener('click', () => void submit());
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') void submit();
    });
    clear.addEventListener('click', () => {
      this.#showError(undefined);
      void this.controller?.clearInput(this.node, this.port).catch(() => {});
    });
  }

  #showError(message: string | undefined): void {
    this.#error.textContent = message ?? '';
    this.#error.classList.toggle('hidden', message === undefined);
  }

  protected update(): void {
    const injected = this.controller?.getInjectedValue(this.node, this.port);
    if (injected === this.#shown) return;
    const before = this.#shown;
    this.#shown = injected;
    this.#injected.replaceChildren();
    if (injected !== undefined) {
      const label = document.createElement('span');
      label.className = 'text-2xs text-wg-text-dim';
      label.textContent = 'Injected:';
      const value = document.createElement('span');
      value.className = 'font-mono text-xs text-wg-success';
      value.textContent = injected;
      this.#injected.append(label, value);
    }
    this.#injected.classList.toggle('hidden', injected === undefined);
    // The box follows what is injected, unless something else is being typed in it.
    const input = this.#input;
    const untouched = input.value === (before ?? '') && this.shadowRoot?.activeElement !== input;
    if (untouched) input.value = injected ?? '';
  }
}

const define = (name: string, ctor: CustomElementConstructor) =>
  customElements.get(name) || customElements.define(name, ctor);
define('witgraph-output-value', WitgraphOutputValue);
define('witgraph-inject-field', WitgraphInjectField);
