import { imageViewer } from './image.ts';
import { seriesViewer } from './series.ts';
import { textViewer } from './text.ts';
import type { PortViewer } from './types.ts';

const viewers = new Map<string, PortViewer>([
  [imageViewer.id, imageViewer],
  [seriesViewer.id, seriesViewer],
  [textViewer.id, textViewer],
]);

/**
 * Makes `viewer` available to every node whose component names it, in
 * place of any registered under the same id (the built-ins are `image`,
 * `series` and `text`). Nodes already showing a preview pick it up when next drawn.
 */
export function registerViewer(viewer: PortViewer): void {
  viewers.set(viewer.id, viewer);
}

/** The viewer registered as `id`, or the `text` viewer when there is none. */
export function viewerFor(id: string): PortViewer {
  return viewers.get(id) ?? textViewer;
}
