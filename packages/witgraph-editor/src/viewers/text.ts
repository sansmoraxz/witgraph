import type { OutputView } from '../controller/types.ts';
import { readable, readableLine } from '../util/readable.ts';
import type { PortViewer } from './types.ts';

/** The longest summary of one value the text viewer shows. */
const MAX_LENGTH = 400;
/** The most stream items the text viewer lists. */
const MAX_ITEMS = 8;

/** Bytes as text when they are printable ASCII, else as hex. */
function bytesText(bytes: Uint8Array): string {
  if (bytes.every((b) => b >= 0x20 && b < 0x7f))
    return JSON.stringify(new TextDecoder().decode(bytes));
  const hex = Array.from(bytes.subarray(0, 16), (b) => b.toString(16).padStart(2, '0')).join(' ');
  return `<${hex}${bytes.length > 16 ? ` … ${bytes.length} bytes` : ''}>`;
}

/**
 * A short rendering of a jco value: JSON, with `BigInt`s as numbers, bytes
 * as text or hex, and other typed arrays by their length.
 */
export function summarize(value: unknown): string {
  if (value === undefined) return '-';
  if (value instanceof Uint8Array) return bytesText(value);
  const text = JSON.stringify(value, (_key, v: unknown) => {
    if (typeof v === 'bigint') return Number(v);
    if (v instanceof Uint8Array) return v.length <= 32 ? bytesText(v) : `<${v.length} bytes>`;
    if (ArrayBuffer.isView(v)) return `<${v.constructor.name}(${v.byteLength} bytes)>`;
    return v;
  });
  const shown = text ?? String(value);
  return shown.length > MAX_LENGTH ? `${shown.slice(0, MAX_LENGTH)}…` : shown;
}

/**
 * What the text viewer shows for an output: a Value's value, as rows; a future's, or that it
 * is pending; a stream's item count and its latest items, one a line.
 */
export function describe(view: OutputView): string {
  switch (view.kind) {
    case 'future':
      if (view.pending)
        return view.count > 0 ? `pending (was ${readableLine(view.latest)})` : 'pending';
      return readable(view.latest);
    case 'stream': {
      if (view.count === 0) return 'no items yet';
      const items = view.recent.slice(-MAX_ITEMS).map(readableLine);
      return [`${view.count} item${view.count === 1 ? '' : 's'}`, ...items].join('\n');
    }
    default:
      return readable(view.latest);
  }
}

/** The output as text: what an output with no viewer of its own shows. */
export const textViewer: PortViewer = {
  id: 'text',
  mount(host) {
    const pre = document.createElement('pre');
    pre.className = 'preview-text';
    host.appendChild(pre);
    return {
      update(view) {
        const text = describe(view);
        if (pre.textContent !== text) pre.textContent = text;
        pre.classList.toggle('pending', view.pending);
        // The newest stream items are at the bottom.
        if (view.kind === 'stream') pre.scrollTop = pre.scrollHeight;
      },
      dispose() {
        pre.remove();
      },
    };
  },
};
