/**
 * About how many bytes `value` holds: its strings and buffers, through its
 * lists and records, and a little for the rest.
 */
export function weight(value: unknown, depth = 0): number {
  if (typeof value === 'string') return 16 + value.length * 2;
  if (ArrayBuffer.isView(value)) return 16 + value.byteLength;
  if (typeof value !== 'object' || value === null) return 8;
  // Deeper than any WIT type nests in practice.
  if (depth > 16) return 16;
  let bytes = 16;
  for (const item of Array.isArray(value) ? value : Object.values(value)) {
    bytes += weight(item, depth + 1);
  }
  return bytes;
}
