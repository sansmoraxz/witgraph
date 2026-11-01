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
/** The narrowest a node can be resized to. */
export const MIN_NODE_WIDTH = 120;
/** The height of a node's preview region until it is resized. */
export const PREVIEW_HEIGHT = 120;
/** The lowest a node's preview region can be resized to. */
export const MIN_PREVIEW_HEIGHT = 40;

/**
 * The height of a node's ports band: the region below its header where its
 * ports sit, above its preview region, if it has one.
 */
export function portsBandHeight(inputCount: number, outputCount: number): number {
  const ports = Math.max(inputCount, outputCount);
  return ports * PORT_SPACING + NODE_PADDING * 2;
}

/** A node's height: header, ports band, then `previewHeight` of preview. */
export function computeNodeHeight(
  inputCount: number,
  outputCount: number,
  previewHeight = 0,
): number {
  return NODE_HEADER_HEIGHT + portsBandHeight(inputCount, outputCount) + previewHeight;
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
