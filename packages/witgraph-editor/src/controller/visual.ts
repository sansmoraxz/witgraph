import { NUMERIC_TYPES, type Port } from 'witgraph-loader';
import {
  computeNodeHeight,
  MIN_NODE_WIDTH,
  MIN_PREVIEW_HEIGHT,
  NODE_HEADER_HEIGHT,
  NODE_WIDTH,
  PREVIEW_HEIGHT,
  portsBandHeight,
} from '../components/styles.ts';
import { hasField } from '../util/svg.ts';
import type { NodePhase, NodePreview, NodeVisual, PortVisual } from './types.ts';

/** A contract's port as the editor shows it. */
export function portVisual(port: Port): PortVisual {
  return {
    name: port.name,
    kind: port.kind,
    // A handle's full `interface.resource` is long: its resource names it.
    typeDisplay: port.handle?.resource ?? port.type_display,
    handle: port.handle,
    optional: port.optional,
    docs: port.docs,
    viewer: port.viewer,
  };
}

/**
 * The viewer for an output its component declares none for: `series` for
 * numbers (one per tick, or per stream item), `text` for anything else.
 */
export function defaultViewer(port: PortVisual): string {
  return NUMERIC_TYPES.has(port.typeDisplay) ? 'series' : 'text';
}

/**
 * The output a node previews, and with what: the one its `editor.preview`
 * attribute names (`none` for no preview), or else the first output its
 * component declares a viewer for.
 */
export function choosePreview(
  outputs: readonly PortVisual[],
  attrs: Readonly<Record<string, string>> = {},
): { port: string; viewer: string } | undefined {
  const named = attrs['editor.preview'];
  if (named === 'none') return undefined;
  const port = named
    ? outputs.find((p) => p.name === named)
    : outputs.find((p) => p.viewer !== undefined);
  return port && { port: port.name, viewer: port.viewer ?? defaultViewer(port) };
}

/** A number attribute, or `undefined` when it is absent or not a number. */
function numberAttr(attrs: Readonly<Record<string, string>>, key: string): number | undefined {
  const value = parseFloat(attrs[key] ?? '');
  return Number.isFinite(value) ? value : undefined;
}

/**
 * A node's size: its `editor.w` and `editor.h` attributes when it was
 * resized, within what its ports and preview need.
 */
export function nodeSize(
  inputs: readonly PortVisual[],
  outputs: readonly PortVisual[],
  attrs: Readonly<Record<string, string>> = {},
): { width: number; height: number; portsHeight: number; preview?: NodePreview } {
  const width = Math.max(MIN_NODE_WIDTH, numberAttr(attrs, 'editor.w') ?? NODE_WIDTH);
  const fields = inputs.filter(hasField).length;
  const slotted = inputs.length - fields;
  const portsHeight = portsBandHeight(slotted, outputs.length, fields);
  const chosen = choosePreview(outputs, attrs);
  if (!chosen) {
    return {
      width,
      portsHeight,
      height: computeNodeHeight(slotted, outputs.length, 0, fields),
    };
  }
  const resized = numberAttr(attrs, 'editor.h');
  const previewHeight = Math.max(
    MIN_PREVIEW_HEIGHT,
    resized === undefined ? PREVIEW_HEIGHT : resized - NODE_HEADER_HEIGHT - portsHeight,
  );
  return {
    width,
    portsHeight,
    height: computeNodeHeight(slotted, outputs.length, previewHeight, fields),
    preview: { ...chosen, height: previewHeight },
  };
}

/** What {@link nodeVisual} builds a node from. */
export interface NodeVisualSource {
  id: string;
  label?: string;
  docs?: string;
  attrs?: Readonly<Record<string, string>>;
  inputs: readonly PortVisual[];
  outputs: readonly PortVisual[];
  phase: NodePhase;
  breakpoint: boolean;
}

/** A node as the canvas draws it: placed and sized by its `editor.*` attributes. */
export function nodeVisual(source: NodeVisualSource): NodeVisual {
  const attrs = source.attrs ?? {};
  return {
    id: source.id,
    x: numberAttr(attrs, 'editor.x') ?? 0,
    y: numberAttr(attrs, 'editor.y') ?? 0,
    ...nodeSize(source.inputs, source.outputs, attrs),
    label: source.label ?? source.id,
    docs: source.docs,
    phase: source.phase,
    breakpoint: source.breakpoint,
    inputs: [...source.inputs],
    outputs: [...source.outputs],
  };
}

export { describeHandle } from 'witgraph-loader';
