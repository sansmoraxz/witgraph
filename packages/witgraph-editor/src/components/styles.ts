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

/** The height of a row holding a Value input's port and its field. */
export const FIELD_ROW_HEIGHT = 24;

/**
 * The height of a node's slots: the region below its header where its outputs, and the
 * inputs that have no field, are spread evenly. A node whose only ports are fields has
 * hardly any.
 */
export function slotsHeight(inputCount: number, outputCount: number, fieldCount = 0): number {
  const rows = Math.max(inputCount, outputCount);
  if (rows === 0 && fieldCount > 0) return NODE_PADDING / 2;
  return rows * PORT_SPACING + NODE_PADDING * 2;
}

/** The height of a node's field rows, below its slots: one for each Value input. */
export function fieldsHeight(fieldCount: number): number {
  return fieldCount > 0 ? fieldCount * FIELD_ROW_HEIGHT + NODE_PADDING / 2 : 0;
}

/**
 * The height of a node's ports band: the region below its header where its
 * ports sit, above its preview region, if it has one. Its slots come first,
 * then a row for each of the `fieldCount` inputs that have a field, which are
 * not among `inputCount`.
 */
export function portsBandHeight(inputCount: number, outputCount: number, fieldCount = 0): number {
  return slotsHeight(inputCount, outputCount, fieldCount) + fieldsHeight(fieldCount);
}

/** A node's height: header, ports band, then `previewHeight` of preview. */
export function computeNodeHeight(
  inputCount: number,
  outputCount: number,
  previewHeight = 0,
  fieldCount = 0,
): number {
  return NODE_HEADER_HEIGHT + portsBandHeight(inputCount, outputCount, fieldCount) + previewHeight;
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
