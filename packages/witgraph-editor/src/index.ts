export type { Diagnostic } from 'witgraph-loader';
export {
  type CompatibleOutput,
  type ConnectionCheck,
  EditorController,
} from './controller/editor-controller.ts';
export type {
  BreakpointState,
  CapabilityInfo,
  EdgeVisual,
  EditorEvent,
  EditorLoadOptions,
  EditorMode,
  GraphJson,
  GraphJsonConnection,
  GraphJsonNode,
  GraphJsonPortRef,
  NodePreview,
  NodeVisual,
  OutputView,
  PortVisual,
  ResourceUse,
  SavedSnapshot,
} from './controller/types.ts';

import { WitgraphCanvas } from './components/canvas.ts';
import { WitgraphCatalog } from './components/catalog.ts';
import { WitgraphControls } from './components/controls.ts';
import { WitgraphInspector } from './components/inspector.ts';
import { WitgraphSource } from './components/source.ts';
import { WitgraphTimeline } from './components/timeline.ts';

export { autoLayout } from './controller/layout.ts';
export { defaultViewer } from './controller/visual.ts';
export { asImage, type ImageValue, imageViewer, sniffImageType } from './viewers/image.ts';
export { registerViewer, viewerFor } from './viewers/registry.ts';
export { numeric, plotPoints, seriesViewer } from './viewers/series.ts';
export { describe, summarize, textViewer } from './viewers/text.ts';
export type { PortViewer, ViewerInstance } from './viewers/types.ts';
export {
  WitgraphCanvas,
  WitgraphCatalog,
  WitgraphControls,
  WitgraphInspector,
  WitgraphSource,
  WitgraphTimeline,
};

const define = (name: string, ctor: CustomElementConstructor) =>
  customElements.get(name) || customElements.define(name, ctor);

export function registerAll(): void {
  define('witgraph-canvas', WitgraphCanvas);
  define('witgraph-controls', WitgraphControls);
  define('witgraph-inspector', WitgraphInspector);
  define('witgraph-timeline', WitgraphTimeline);
  define('witgraph-source', WitgraphSource);
  define('witgraph-catalog', WitgraphCatalog);
}
