import type { EditorController } from '../controller/editor-controller.ts';
import { describeHandle } from '../controller/visual.ts';
import { summarize } from '../viewers/text.ts';
import { readableElement, readableLine } from './readable.ts';
import { weight } from './weight.ts';

/** The biggest value written out whole, in bytes. */
export const MAX_WAVE_BYTES = 64 * 1024;

/**
 * What an output shows: for a stream or a future, what it is doing; and its latest value, or
 * text standing for it (a handle's description, a summary of a value too big to write out,
 * whose text could take seconds, and hundreds of megabytes).
 */
interface Shown {
  doing?: string;
  value?: unknown;
  text?: string;
}

function shown(controller: EditorController, node: string, port: string): Shown {
  const value = controller.latestOutput(node, port);
  const visual = controller.nodes.get(node);
  const output = visual?.outputs.find((p) => p.name === port);
  // What moves to its reader has no value to read: say what it is doing instead.
  if (output?.kind === 'stream' || output?.kind === 'future') {
    if (visual?.phase === 'pending') return { text: '-' };
    const running = visual?.phase === 'running';
    const doing =
      output.kind === 'stream'
        ? running
          ? 'streaming'
          : 'stream ended'
        : running
          ? 'waiting'
          : 'settled';
    return value === undefined ? { doing } : { doing, value };
  }
  if (value === undefined) return { text: '-' };
  const handle = output?.handle;
  // A handle has no text: its object may describe itself.
  if (handle) return { text: describeHandle(value, handle.resource) };
  if (weight(value) > MAX_WAVE_BYTES) return { text: `${summarize(value)} (too large to show)` };
  return { value };
}

/** An output on one line, as people read it ([`readableLine`](./readable.ts)). */
export function outputText(controller: EditorController, node: string, port: string): string {
  const { doing, value, text } = shown(controller, node, port);
  const what = text ?? (value === undefined ? undefined : readableLine(value));
  if (doing === undefined) return what ?? '-';
  return what === undefined ? doing : `${doing}, latest: ${what}`;
}

/**
 * An output as elements ([`readableElement`](./readable.ts)): a record's fields in a grid,
 * under what a stream or a future is doing.
 */
export function outputElement(
  controller: EditorController,
  node: string,
  port: string,
): HTMLElement {
  const { doing, value, text } = shown(controller, node, port);
  const element = document.createElement('div');
  if (doing !== undefined) {
    const status = document.createElement('div');
    status.className = 'readable-note';
    status.textContent = value === undefined ? doing : `${doing}, latest:`;
    element.append(status);
  }
  if (text !== undefined) element.append(text);
  else if (value !== undefined) element.append(readableElement(value));
  return element;
}
