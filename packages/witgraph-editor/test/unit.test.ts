import assert from 'node:assert/strict';
import { test } from 'node:test';

import { edgePath, portPosition } from '../src/util/svg.ts';
import {
  computeNodeHeight,
  NODE_HEADER_HEIGHT,
  NODE_PADDING,
  PORT_SPACING,
} from '../src/components/styles.ts';
import { autoLayout } from '../src/controller/layout.ts';

// --- edgePath ----------------------------------------------------------------

test('edgePath returns a valid SVG path starting with M and containing C', () => {
  const path = edgePath(0, 0, 100, 50);
  assert.ok(path.startsWith('M '));
  assert.ok(path.includes(' C '));
});

test('edgePath horizontal left-to-right produces reasonable control points', () => {
  const path = edgePath(0, 100, 200, 100);
  // dx = |200 - 0| * 0.4 = 80; cp1 = (80, 100), cp2 = (120, 100)
  assert.equal(path, 'M 0,100 C 80,100 120,100 200,100');
});

test('edgePath same point produces a degenerate straight path', () => {
  const path = edgePath(50, 50, 50, 50);
  // dx = 0, so control points collapse onto start/end
  assert.equal(path, 'M 50,50 C 50,50 50,50 50,50');
});

// --- portPosition ------------------------------------------------------------

test('portPosition input port is on the left edge', () => {
  const pos = portPosition(10, 20, 180, 100, 0, 1, 'input');
  assert.equal(pos.x, 10);
});

test('portPosition output port is on the right edge', () => {
  const pos = portPosition(10, 20, 180, 100, 0, 1, 'output');
  assert.equal(pos.x, 10 + 180);
});

test('portPosition ports are distributed vertically within the node height', () => {
  const nodeY = 20;
  const nodeHeight = 100;
  for (let i = 0; i < 3; i++) {
    const pos = portPosition(10, nodeY, 180, nodeHeight, i, 3, 'input');
    assert.ok(pos.y > nodeY, `port ${i} y should be above nodeY`);
    assert.ok(pos.y < nodeY + nodeHeight, `port ${i} y should be below bottom`);
  }
});

test('portPosition single port is centered in the body area below header', () => {
  const pos = portPosition(0, 0, 180, 100, 0, 1, 'input');
  // portAreaTop = NODE_HEADER_HEIGHT (32), portAreaH = 68, y = 32 + 68/2 = 66
  assert.equal(pos.y, 32 + (100 - 32) / 2);
});

test('portPosition multiple ports are evenly spaced in the body area', () => {
  const positions = [0, 1, 2].map((i) =>
    portPosition(0, 0, 180, 100, i, 3, 'input'),
  );
  // portAreaTop = 32, portAreaH = 68, y = 32 + 68*(i+1)/4
  const gap1 = positions[1].y - positions[0].y;
  const gap2 = positions[2].y - positions[1].y;
  assert.equal(gap1, gap2);
});

// --- computeNodeHeight -------------------------------------------------------

test('computeNodeHeight zero ports gives header + padding only', () => {
  const height = computeNodeHeight(0, 0);
  assert.equal(height, NODE_HEADER_HEIGHT + NODE_PADDING * 2);
});

test('computeNodeHeight scales with max(inputs, outputs)', () => {
  const h3 = computeNodeHeight(1, 3);
  const h5 = computeNodeHeight(5, 2);
  assert.equal(h3, NODE_HEADER_HEIGHT + 3 * PORT_SPACING + NODE_PADDING * 2);
  assert.equal(h5, NODE_HEADER_HEIGHT + 5 * PORT_SPACING + NODE_PADDING * 2);
  assert.ok(h5 > h3);
});

test('computeNodeHeight symmetric inputs/outputs give same height', () => {
  assert.equal(computeNodeHeight(3, 5), computeNodeHeight(5, 3));
  assert.equal(computeNodeHeight(4, 4), computeNodeHeight(4, 4));
});

// --- autoLayout --------------------------------------------------------------

test('autoLayout two connected nodes: source is left of target', async () => {
  const nodes = [
    { id: 'a', width: 180, height: 100, inputs: [], outputs: [{ name: 'out' }] },
    { id: 'b', width: 180, height: 100, inputs: [{ name: 'in' }], outputs: [] },
  ];
  const edges = [
    { id: 'c', fromNode: 'a', fromPort: 'out', toNode: 'b', toPort: 'in' },
  ];
  const positions = await autoLayout(nodes, edges);
  assert.equal(positions.size, 2);
  assert.ok(positions.get('a')!.x < positions.get('b')!.x, 'source should be left of target');
});

test('autoLayout single node gets a position', async () => {
  const positions = await autoLayout(
    [{ id: 'x', width: 180, height: 100, inputs: [], outputs: [] }],
    [],
  );
  assert.equal(positions.size, 1);
  assert.ok(positions.has('x'));
});

test('autoLayout positions are non-negative', async () => {
  const nodes = [
    { id: 'a', width: 180, height: 100, inputs: [], outputs: [{ name: 'out' }] },
    { id: 'b', width: 180, height: 100, inputs: [{ name: 'in' }], outputs: [] },
  ];
  const edges = [
    { id: 'c', fromNode: 'a', fromPort: 'out', toNode: 'b', toPort: 'in' },
  ];
  const positions = await autoLayout(nodes, edges);
  for (const [, pos] of positions) {
    assert.ok(pos.x >= 0, 'x should be non-negative');
    assert.ok(pos.y >= 0, 'y should be non-negative');
  }
});
