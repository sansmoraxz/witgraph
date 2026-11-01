/**
 * Default node dimensions — used as fallbacks when CSS custom properties
 * (`--wg-node-width`, `--wg-port-radius`, etc.) are not set.
 *
 * Consumers can override these via CSS custom properties on an ancestor
 * of the editor components.
 */
export const NODE_WIDTH = 180;
export const PORT_RADIUS = 6;
export const NODE_HEADER_HEIGHT = 32;
export const PORT_SPACING = 22;
export const NODE_PADDING = 8;
export const NODE_RADIUS = 8;

export function computeNodeHeight(inputCount: number, outputCount: number): number {
  const ports = Math.max(inputCount, outputCount);
  return NODE_HEADER_HEIGHT + ports * PORT_SPACING + NODE_PADDING * 2;
}

/**
 * Resolves a CSS `--wg-*` sizing property to a number (stripping `px`),
 * falling back to the given default when the property is not set.
 */
export function resolveSizing(el: Element, varName: string, fallback: number): number {
  const raw = getComputedStyle(el).getPropertyValue(varName).trim();
  if (!raw) return fallback;
  const n = parseFloat(raw);
  return Number.isFinite(n) ? n : fallback;
}
