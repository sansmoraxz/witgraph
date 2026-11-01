import { NODE_HEADER_HEIGHT } from '../components/styles.ts';

export function edgePath(
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): string {
  const dx = Math.abs(x2 - x1) * 0.4;
  const cp1x = x1 + dx;
  const cp1y = y1;
  const cp2x = x2 - dx;
  const cp2y = y2;
  return `M ${x1},${y1} C ${cp1x},${cp1y} ${cp2x},${cp2y} ${x2},${y2}`;
}

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
