import type { ELK as ELKInstance, ElkNode, ElkExtendedEdge, ElkPort } from 'elkjs/lib/elk-api.js';
import ELKFactory from 'elkjs/lib/elk.bundled.js';
import { NODE_HEADER_HEIGHT } from '../components/styles.ts';

export interface LayoutNode {
  id: string;
  width: number;
  height: number;
  inputs: { name: string }[];
  outputs: { name: string }[];
}

export interface LayoutEdge {
  id: string;
  fromNode: string;
  fromPort: string;
  toNode: string;
  toPort: string;
}

function portId(nodeId: string, side: 'in' | 'out', name: string): string {
  return `${nodeId}:${side}:${name}`;
}

function buildPorts(
  nodeId: string,
  ports: { name: string }[],
  side: 'in' | 'out',
  nodeWidth: number,
  nodeHeight: number,
): ElkPort[] {
  const count = ports.length;
  const portSide = side === 'in' ? 'WEST' : 'EAST';
  const x = side === 'in' ? 0 : nodeWidth;

  return ports.map((p, i) => {
    const y = NODE_HEADER_HEIGHT + ((nodeHeight - NODE_HEADER_HEIGHT) * (i + 1)) / (count + 1);
    return {
      id: portId(nodeId, side, p.name),
      x,
      y,
      width: 1,
      height: 1,
      layoutOptions: {
        'org.eclipse.elk.port.side': portSide,
      },
    };
  });
}

export async function autoLayout(
  nodes: LayoutNode[],
  edges: LayoutEdge[],
): Promise<Map<string, { x: number; y: number }>> {
  const ELK = ELKFactory as unknown as new () => ELKInstance;
  const elk = new ELK();

  const elkNodes: ElkNode[] = nodes.map((n) => ({
    id: n.id,
    width: n.width,
    height: n.height,
    ports: [
      ...buildPorts(n.id, n.inputs, 'in', n.width, n.height),
      ...buildPorts(n.id, n.outputs, 'out', n.width, n.height),
    ],
    layoutOptions: {
      'org.eclipse.elk.portConstraints': 'FIXED_SIDE',
    },
  }));

  const elkEdges: ElkExtendedEdge[] = edges.map((e) => ({
    id: e.id,
    sources: [portId(e.fromNode, 'out', e.fromPort)],
    targets: [portId(e.toNode, 'in', e.toPort)],
  }));

  const graph: ElkNode = {
    id: 'root',
    children: elkNodes,
    edges: elkEdges,
    layoutOptions: {
      'org.eclipse.elk.algorithm': 'layered',
      'org.eclipse.elk.direction': 'RIGHT',
      'org.eclipse.elk.layered.spacing.nodeNodeBetweenLayers': '100',
      'org.eclipse.elk.spacing.nodeNode': '40',
    },
  };

  const result = await elk.layout(graph);
  const positions = new Map<string, { x: number; y: number }>();

  for (const child of result.children ?? []) {
    positions.set(child.id, { x: child.x ?? 0, y: child.y ?? 0 });
  }

  return positions;
}
