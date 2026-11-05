import type { OutputView, PortVisual } from '../controller/types.ts';

/** A viewer mounted in one node's preview region. */
export interface ViewerInstance {
  /**
   * Shows what the output has carried: its latest value, those before it
   * and how many. Called when the graph is drawn, and once a frame at most
   * while the output's values change.
   */
  update(view: OutputView): void;
  /** Removes what the viewer put in its host and releases what it holds. */
  dispose(): void;
}

/**
 * How a node previews one of its outputs. A component names its viewer in
 * its `witgraph-viewer` custom section (`output.frames=image`); the editor
 * looks the name up among the registered viewers.
 */
export interface PortViewer {
  /** The name components refer to the viewer by. */
  id: string;
  /**
   * Builds the viewer inside `host`, an element filling the node's preview
   * region. Style it with `--wg-*` custom properties only: the editor ships
   * no theme.
   */
  mount(host: HTMLElement, port: PortVisual): ViewerInstance;
}
