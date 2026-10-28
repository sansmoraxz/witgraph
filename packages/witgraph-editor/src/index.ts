export { EditorController } from './controller/editor-controller.ts';
export type {
  EditorMode,
  NodeVisual,
  EdgeVisual,
  PortVisual,
  EditorEvent,
  OutputSnapshot,
  BreakpointState,
  GraphJson,
  GraphJsonNode,
  GraphJsonConnection,
  GraphJsonPortRef,
  EditorLoadOptions,
} from './controller/types.ts';

import { WitgraphCanvas } from './components/canvas.ts';
import { WitgraphControls } from './components/controls.ts';
import { WitgraphInspector } from './components/inspector.ts';
import { WitgraphTimeline } from './components/timeline.ts';
import { WitgraphCatalog } from './components/catalog.ts';

export { WitgraphCanvas, WitgraphControls, WitgraphInspector, WitgraphTimeline, WitgraphCatalog };

export { autoLayout } from './controller/layout.ts';

const define = (name: string, ctor: CustomElementConstructor) =>
  customElements.get(name) || customElements.define(name, ctor);

export function registerAll(): void {
  define('witgraph-canvas', WitgraphCanvas);
  define('witgraph-controls', WitgraphControls);
  define('witgraph-inspector', WitgraphInspector);
  define('witgraph-timeline', WitgraphTimeline);
  define('witgraph-catalog', WitgraphCatalog);
}
