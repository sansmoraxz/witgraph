import { FIELD_ROW_HEIGHT, NODE_HEADER_HEIGHT, slotsHeight } from '../components/styles.ts';

export function edgePath(x1: number, y1: number, x2: number, y2: number): string {
  const dx = Math.abs(x2 - x1) * 0.4;
  const cp1x = x1 + dx;
  const cp1y = y1;
  const cp2x = x2 - dx;
  const cp2y = y2;
  return `M ${x1},${y1} C ${cp1x},${cp1y} ${cp2x},${cp2y} ${x2},${y2}`;
}

/**
 * Where a node's port sits: on its left (input) or right (output) edge,
 * spread evenly below its header over `nodeHeight`. Pass the height down to
 * the bottom of the node's ports band, not the whole node: a preview region
 * below it holds no ports.
 */
export function portPosition(
  nodeX: number,
  nodeY: number,
  nodeWidth: number,
  nodeHeight: number,
  portIndex: number,
  portCount: number,
  side: 'input' | 'output',
): { x: number; y: number } {
  const x = side === 'input' ? nodeX : nodeX + nodeWidth;
  const portAreaTop = nodeY + NODE_HEADER_HEIGHT;
  const portAreaH = nodeHeight - NODE_HEADER_HEIGHT;
  const y = portAreaTop + (portAreaH * (portIndex + 1)) / (portCount + 1);
  return { x, y };
}

/** Whether an input has a field on its node: each Value input does, for its value. */
export function hasField(port: { kind: string }): boolean {
  return port.kind === 'value';
}

/** What placing a node's ports needs of it. */
export interface PortGeometry {
  width: number;
  inputs: readonly { name: string; kind: string }[];
  outputs: readonly { name: string }[];
}

/**
 * Where a node's port sits, from the node's top left corner: outputs, and the inputs
 * that have no field, spread over its slots, and each input with a field on its row,
 * below them. `undefined` for a port the node does not have.
 */
export function portOffset(
  node: PortGeometry,
  side: 'input' | 'output',
  name: string,
): { x: number; y: number } | undefined {
  const fields = node.inputs.filter(hasField);
  const slotted = node.inputs.filter((p) => !hasField(p));
  const slots = slotsHeight(slotted.length, node.outputs.length, fields.length);
  if (side === 'input') {
    const row = fields.findIndex((p) => p.name === name);
    if (row >= 0) {
      return { x: 0, y: NODE_HEADER_HEIGHT + slots + (row + 0.5) * FIELD_ROW_HEIGHT };
    }
  }
  const ports = side === 'input' ? slotted : node.outputs;
  const index = ports.findIndex((p) => p.name === name);
  if (index < 0) return undefined;
  return portPosition(0, 0, node.width, NODE_HEADER_HEIGHT + slots, index, ports.length, side);
}
