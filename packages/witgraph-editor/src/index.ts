export { EditorController } from './controller/editor-controller.ts';
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
  NodeVisual,
  OutputSnapshot,
  PortVisual,
  SavedSnapshot,
} from './controller/types.ts';

import { WitgraphCanvas } from './components/canvas.ts';
import { WitgraphCatalog } from './components/catalog.ts';
import { WitgraphControls } from './components/controls.ts';
import { WitgraphInspector } from './components/inspector.ts';
import { WitgraphTimeline } from './components/timeline.ts';

export { autoLayout } from './controller/layout.ts';
export { WitgraphCanvas, WitgraphCatalog, WitgraphControls, WitgraphInspector, WitgraphTimeline };

const define = (name: string, ctor: CustomElementConstructor) =>
  customElements.get(name) || customElements.define(name, ctor);

export function registerAll(): void {
  define('witgraph-canvas', WitgraphCanvas);
  define('witgraph-controls', WitgraphControls);
  define('witgraph-inspector', WitgraphInspector);
  define('witgraph-timeline', WitgraphTimeline);
  define('witgraph-catalog', WitgraphCatalog);
}
