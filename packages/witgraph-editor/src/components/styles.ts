export const NODE_WIDTH = 180;
export const PORT_RADIUS = 6;
export const NODE_HEADER_HEIGHT = 32;
export const PORT_SPACING = 22;
export const NODE_PADDING = 8;

export function computeNodeHeight(inputCount: number, outputCount: number): number {
  const ports = Math.max(inputCount, outputCount);
  return NODE_HEADER_HEIGHT + ports * PORT_SPACING + NODE_PADDING * 2;
}
