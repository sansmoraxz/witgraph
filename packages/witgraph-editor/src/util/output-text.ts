import type { EditorController } from '../controller/editor-controller.ts';
import { describeHandle } from '../controller/visual.ts';
import { summarize } from '../viewers/text.ts';
import { weight } from './weight.ts';

/** The biggest value written out as WAVE text, in bytes. */
export const MAX_WAVE_BYTES = 64 * 1024;

/**
 * An output as text: a Value output as WAVE text, or a summary of it when it
 * is too big to write out (its text could take seconds, and hundreds of
 * megabytes), and for a stream or a future, what it is doing.
 */
export function outputText(controller: EditorController, node: string, port: string): string {
  const value = controller.latestOutput(node, port);
  const visual = controller.nodes.get(node);
  const output = visual?.outputs.find((p) => p.name === port);
  // What moves to its reader has no value to read: say what it is doing instead.
  if (output?.kind === 'stream' || output?.kind === 'future') {
    if (visual?.phase === 'pending') return '-';
    const running = visual?.phase === 'running';
    const doing =
      output.kind === 'stream'
        ? running
          ? 'streaming'
          : 'stream ended'
        : running
          ? 'waiting'
          : 'settled';
    return value === undefined ? doing : `${doing}, latest: ${summarize(value)}`;
  }
  if (value === undefined) return '-';
  const handle = output?.handle;
  // A handle has no text: its object may describe itself.
  if (handle) return describeHandle(value, handle.resource);
  if (weight(value) > MAX_WAVE_BYTES) return `${summarize(value)} (too large to show as WAVE)`;
  return controller.outputWave(node, port) ?? '…';
}
