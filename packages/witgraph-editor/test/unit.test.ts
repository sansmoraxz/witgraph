import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  computeNodeHeight,
  FIELD_ROW_HEIGHT,
  fieldsHeight,
  MIN_NODE_WIDTH,
  MIN_PREVIEW_HEIGHT,
  NODE_HEADER_HEIGHT,
  NODE_PADDING,
  NODE_WIDTH,
  PORT_SPACING,
  PREVIEW_HEIGHT,
  portsBandHeight,
  slotsHeight,
} from '../src/components/styles.ts';
import { autoLayout } from '../src/controller/layout.ts';
import type { OutputView, PortVisual } from '../src/controller/types.ts';
import { choosePreview, defaultViewer, nodeSize, nodeVisual } from '../src/controller/visual.ts';
import { edgePath, portOffset, portPosition } from '../src/util/svg.ts';
import { asImage, sniffImageType } from '../src/viewers/image.ts';
import { numeric, plotPoints, plotted } from '../src/viewers/series.ts';
import { describe, summarize } from '../src/viewers/text.ts';

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
  const positions = [0, 1, 2].map((i) => portPosition(0, 0, 180, 100, i, 3, 'input'));
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
  const edges = [{ id: 'c', fromNode: 'a', fromPort: 'out', toNode: 'b', toPort: 'in' }];
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
  const edges = [{ id: 'c', fromNode: 'a', fromPort: 'out', toNode: 'b', toPort: 'in' }];
  const positions = await autoLayout(nodes, edges);
  for (const [, pos] of positions) {
    assert.ok(pos.x >= 0, 'x should be non-negative');
    assert.ok(pos.y >= 0, 'y should be non-negative');
  }
});

// --- Previews ----------------------------------------------------------------

const port = (name: string, viewer?: string): PortVisual => ({
  name,
  kind: 'stream',
  typeDisplay: 'frame',
  optional: false,
  viewer,
});

test('computeNodeHeight adds the preview region below the ports band', () => {
  assert.equal(computeNodeHeight(2, 1, 50), NODE_HEADER_HEIGHT + portsBandHeight(2, 1) + 50);
});

test('a node previews the first output its component declares a viewer for', () => {
  const outputs = [port('count'), port('frames', 'image'), port('thumb', 'image')];
  assert.deepEqual(choosePreview(outputs), { port: 'frames', viewer: 'image' });
  assert.equal(choosePreview([port('count')]), undefined);
  // The node's attribute picks another, or none; one without a viewer shows as text.
  assert.deepEqual(choosePreview(outputs, { 'editor.preview': 'thumb' }), {
    port: 'thumb',
    viewer: 'image',
  });
  assert.deepEqual(choosePreview(outputs, { 'editor.preview': 'count' }), {
    port: 'count',
    viewer: 'text',
  });
  // Numbers are plotted.
  const numbers = { ...port('n'), typeDisplay: 'u32' };
  assert.equal(defaultViewer(numbers), 'series');
  assert.deepEqual(choosePreview([numbers], { 'editor.preview': 'n' }), {
    port: 'n',
    viewer: 'series',
  });
  assert.equal(choosePreview(outputs, { 'editor.preview': 'none' }), undefined);
  assert.equal(choosePreview(outputs, { 'editor.preview': 'missing' }), undefined);
});

test('a previewing node is sized by its attributes, within its minimums', () => {
  const inputs = [port('frames')];
  const outputs = [port('frames', 'image')];
  const band = portsBandHeight(1, 1);
  const fresh = nodeSize(inputs, outputs);
  assert.equal(fresh.width, NODE_WIDTH);
  assert.equal(fresh.portsHeight, band);
  assert.deepEqual(fresh.preview, { port: 'frames', viewer: 'image', height: PREVIEW_HEIGHT });
  assert.equal(fresh.height, NODE_HEADER_HEIGHT + band + PREVIEW_HEIGHT);

  const resized = nodeSize(inputs, outputs, { 'editor.w': '300', 'editor.h': '400' });
  assert.equal(resized.width, 300);
  assert.equal(resized.height, 400);
  assert.equal(resized.preview?.height, 400 - NODE_HEADER_HEIGHT - band);

  const tiny = nodeSize(inputs, outputs, { 'editor.w': '10', 'editor.h': '10' });
  assert.equal(tiny.width, MIN_NODE_WIDTH);
  assert.equal(tiny.preview?.height, MIN_PREVIEW_HEIGHT);

  // Without a preview, the height is the ports' whatever was stored.
  const plain = nodeSize(inputs, [port('count')], { 'editor.h': '400' });
  assert.equal(plain.preview, undefined);
  assert.equal(plain.height, computeNodeHeight(1, 1));
});

test('nodeVisual places a node from its attributes', () => {
  const visual = nodeVisual({
    id: 'n',
    attrs: { 'editor.x': '12.5', 'editor.y': 'nope' },
    inputs: [],
    outputs: [port('frames', 'image')],
    phase: 'idle',
    breakpoint: false,
  });
  assert.deepEqual([visual.x, visual.y, visual.label], [12.5, 0, 'n']);
  assert.equal(visual.preview?.port, 'frames');
});

test('portPosition keeps ports in the band above the preview region', () => {
  const node = nodeSize([port('a'), port('b')], [port('frames', 'image')]);
  const bottom = NODE_HEADER_HEIGHT + node.portsHeight;
  for (let i = 0; i < 2; i++) {
    const { y } = portPosition(0, 0, node.width, bottom, i, 2, 'input');
    assert.ok(y > NODE_HEADER_HEIGHT && y < bottom, `port ${i} at ${y}`);
  }
});

test('each Value input has a row of its own, below the ports spread over the slots', () => {
  const value = (name: string): PortVisual => ({
    ...port(name),
    kind: 'value',
    typeDisplay: 'u32',
  });
  const inputs = [value('a'), port('frames'), value('b')];
  const outputs = [port('out')];
  const node = { ...nodeSize(inputs, outputs), inputs, outputs };
  // The slots hold one row, for `frames` and `out`; then a row for each field.
  const slots = slotsHeight(1, 1, 2);
  assert.equal(node.portsHeight, slots + fieldsHeight(2));
  assert.equal(node.height, computeNodeHeight(1, 1, 0, 2));

  const at = (side: 'input' | 'output', name: string) => portOffset(node, side, name)?.y;
  assert.equal(at('input', 'frames'), NODE_HEADER_HEIGHT + slots / 2);
  assert.equal(at('output', 'out'), NODE_HEADER_HEIGHT + slots / 2);
  assert.equal(at('input', 'a'), NODE_HEADER_HEIGHT + slots + FIELD_ROW_HEIGHT / 2);
  assert.equal(at('input', 'b'), NODE_HEADER_HEIGHT + slots + (FIELD_ROW_HEIGHT * 3) / 2);
  assert.equal(portOffset(node, 'input', 'b')?.x, 0);
  assert.equal(portOffset(node, 'output', 'out')?.x, node.width);
  assert.equal(portOffset(node, 'input', 'missing'), undefined);
  // Every row is above the bottom of the ports band.
  assert.ok((at('input', 'b') ?? Infinity) < NODE_HEADER_HEIGHT + node.portsHeight);

  // A node of fields alone has hardly any slots.
  const sink = { ...nodeSize([value('a')], []), inputs: [value('a')], outputs: [] };
  assert.equal(sink.portsHeight, slotsHeight(0, 0, 1) + FIELD_ROW_HEIGHT + NODE_PADDING / 2);
  assert.ok(slotsHeight(0, 0, 1) < slotsHeight(0, 0));
});

test('encoded images are known by their leading bytes', () => {
  const png = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0);
  const jpeg = Uint8Array.of(0xff, 0xd8, 0xff, 0xe0);
  const gif = new TextEncoder().encode('GIF89a');
  const webp = new TextEncoder().encode('RIFF\0\0\0\0WEBPVP8 ');
  assert.equal(sniffImageType(png), 'image/png');
  assert.equal(sniffImageType(jpeg), 'image/jpeg');
  assert.equal(sniffImageType(gif), 'image/gif');
  assert.equal(sniffImageType(webp), 'image/webp');
  assert.equal(sniffImageType(Uint8Array.of(1, 2, 3)), undefined);
  assert.equal(sniffImageType(png.subarray(0, 4)), undefined);
});

test('asImage takes RGBA frames and encoded bytes, and nothing else', () => {
  const pixels = new Uint8Array(2 * 3 * 4).fill(7);
  const frame = asImage({ width: 2, height: 3, pixels });
  assert.equal(frame?.kind, 'rgba');
  if (frame?.kind === 'rgba') {
    assert.deepEqual([frame.width, frame.height, frame.pixels.length], [2, 3, 24]);
  }
  assert.equal(asImage({ width: 2, height: 3, pixels: Array(24).fill(1) })?.kind, 'rgba');
  assert.equal(asImage({ width: 2, height: 2, pixels }), undefined, 'the wrong size');
  assert.equal(asImage({ width: 0, height: 0, pixels: new Uint8Array() }), undefined);
  assert.equal(asImage(Uint8Array.of(0xff, 0xd8, 0xff))?.kind, 'encoded');
  for (const other of [undefined, null, 3, 'image', Uint8Array.of(1), { width: 1 }]) {
    assert.equal(asImage(other), undefined, String(other));
  }
});

test('the text viewer summarizes jco values', () => {
  assert.equal(summarize(undefined), '-');
  assert.equal(summarize({ a: 1n, b: 'x' }), '{"a":1,"b":"x"}');
  // Bytes read as text when they are printable, else as hex.
  assert.equal(summarize(new TextEncoder().encode('temp:23')), '"temp:23"');
  assert.equal(summarize(Uint8Array.of(0, 255)), '<00 ff>');
  assert.equal(summarize(new Uint8Array(20)).endsWith('… 20 bytes>'), true);
  assert.equal(summarize(new Float32Array(2)), '"<Float32Array(8 bytes)>"');
  assert.ok(summarize('x'.repeat(1000)).endsWith('…'));
});

const view = (view: Partial<OutputView>): OutputView => ({
  kind: 'value',
  latest: undefined,
  recent: [],
  count: 0,
  pending: false,
  ...view,
});

test('the text viewer shows a future pending, then its value', () => {
  assert.equal(describe(view({ kind: 'future', pending: true })), 'pending');
  assert.equal(describe(view({ kind: 'future', latest: 7, count: 1 })), '7');
  assert.equal(
    describe(view({ kind: 'future', latest: 7, count: 1, pending: true })),
    'pending (was 7)',
  );
});

test("the text viewer lists a stream's count and latest items", () => {
  assert.equal(describe(view({ kind: 'stream' })), 'no items yet');
  const recent = Array.from({ length: 20 }, (_, i) => i);
  const text = describe(view({ kind: 'stream', latest: 19, recent, count: 100 }));
  assert.deepEqual(text.split('\n'), ['100 items', '12', '13', '14', '15', '16', '17', '18', '19']);
});

test("the series viewer plots numbers, records' first numbers and bools", () => {
  assert.equal(numeric(3), 3);
  assert.equal(numeric(5n), 5);
  assert.equal(numeric(true), 1);
  assert.equal(numeric({ label: 'x', value: 2.5, other: 9 }), 2.5);
  for (const other of ['3', Number.NaN, null, undefined, {}, Uint8Array.of(1), [1]]) {
    assert.equal(numeric(other), undefined, String(other));
  }
  assert.equal(plotPoints([]), '');
  assert.equal(plotPoints([4]), '0,20 100,20', 'one value is a level line');
  assert.equal(plotPoints([0, 5, 10]), '0,40 50,20 100,0');
  assert.equal(plotPoints([2, 2]), '0,20 100,20', 'a flat line in the middle');
});

test('the series viewer looks into a record with no number of its own, and plots a list whole', () => {
  // A sample, as jco lifts it: its value is a variant's payload.
  const sample = (v: number) => ({ io: 'T1', quality: 'good', value: { tag: 'float', val: v } });
  assert.equal(numeric(sample(22.5)), 22.5);
  assert.equal(numeric({ source: 'write', sample: sample(3) }), 3, 'a record in a record');
  assert.equal(numeric({ io: 'T3', quality: 'bad' }), undefined);
  assert.equal(numeric({ count: 2, value: { tag: 'float', val: 9 } }), 2, 'its own number first');
  // A Value output that is a list is one line; anything else, a point per value.
  const history = [sample(1), sample(2), { io: 'T1', quality: 'bad' }, sample(3)];
  assert.deepEqual(plotted({ kind: 'value', latest: history, recent: [history] }), [1, 2, 3]);
  assert.deepEqual(plotted({ kind: 'value', latest: 5, recent: [4, 5] }), [4, 5]);
  assert.deepEqual(plotted({ kind: 'stream', latest: [7], recent: [[6], [7]] }), []);
});
