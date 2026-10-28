import type { ComponentSource, Capability, PortValues, Wasm } from 'witgraph-loader';

// ---------------------------------------------------------------------------
// Editor state
// ---------------------------------------------------------------------------

/** Overall mode of the editor controller. */
export type EditorMode = 'idle' | 'running' | 'paused' | 'error';

/** Lifecycle phase of a single node, mirroring the scheduler's phases. */
export type NodePhase = 'pending' | 'running' | 'idle' | 'faulted' | 'cancelled';

// ---------------------------------------------------------------------------
// Visual models
// ---------------------------------------------------------------------------

/** A port on a node, as the editor renders it. */
export interface PortVisual {
  name: string;
  kind: 'value' | 'stream' | 'future';
  typeDisplay: string;
  optional: boolean;
  docs?: string;
}

/** A node on the canvas, combining layout, contract, and runtime state. */
export interface NodeVisual {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  label: string;
  phase: NodePhase;
  fault?: { kind: string; message: string };
  culprit?: string;
  breakpoint: boolean;
  expanded: boolean;
  inputs: PortVisual[];
  outputs: PortVisual[];
}

/** A connection between two ports, as the editor renders it. */
export interface EdgeVisual {
  id: string;
  fromNode: string;
  fromPort: string;
  toNode: string;
  toPort: string;
  feedback: boolean;
}

// ---------------------------------------------------------------------------
// Runtime observations
// ---------------------------------------------------------------------------

/** One trace event taken from the scheduler, or an editor-level error. */
export interface TraceEntry {
  event: string;
  timestamp: number;
  node?: string;
  island?: number;
  generation?: number;
  from?: string;
  to?: string;
  fault?: { kind: string; message: string };
  message?: string;
  port?: string;
  count?: number;
}

/** A fault report taken from the scheduler after a tick. */
export interface FaultReport {
  members: string[];
  culprit?: string;
  fault: { kind: string; message: string };
}

/** The per-node state returned by `WebGraph.nodeState`. */
export interface NodeState {
  phase: NodePhase;
  fault?: { kind: string; message: string };
  culprit?: string;
}

// ---------------------------------------------------------------------------
// Custom events
// ---------------------------------------------------------------------------

/** Discriminator for events the controller dispatches. */
export interface EditorEvent {
  type: 'state-changed' | 'trace-event' | 'fault' | 'output-changed';
}

/** A recorded output value at a given tick. */
export interface OutputSnapshot {
  tick: number;
  wave: string;
}

/** A reversible editing command for undo/redo. */
export interface EditorCommand {
  execute(): void;
  undo(): void;
  description: string;
}

/** Breakpoint state: what node is paused and its input values. */
export interface BreakpointState {
  node: string;
  inputs: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Graph JSON — matches witgraph-ir serialization
// ---------------------------------------------------------------------------

/** A reference to one port on one node (`PortRef` in the IR). */
export interface GraphJsonPortRef {
  node: string;
  port: string;
}

/** A node in the serialized graph (`Node` in the IR). */
export interface GraphJsonNode {
  id: string;
  /** The `ComponentRef` this node instantiates (opaque to the editor). */
  component: object;
  label?: string;
  resources?: Record<string, unknown>;
  attrs?: Record<string, string>;
}

/** A connection in the serialized graph (`Connection` in the IR). */
export interface GraphJsonConnection {
  id: string;
  from: GraphJsonPortRef;
  to: GraphJsonPortRef;
  feedback?: boolean;
  attrs?: Record<string, string>;
}

/** A link in the serialized graph (`Link` in the IR). */
export interface GraphJsonLink {
  id: string;
  node: string;
  import: string;
  provider: object;
  export: string;
}

/** The full serialized graph (`Graph` in the IR). */
export interface GraphJson {
  metadata: {
    name: string;
    description?: string;
    attrs?: Record<string, string>;
  };
  components: object[];
  nodes: GraphJsonNode[];
  connections: GraphJsonConnection[];
  links?: GraphJsonLink[];
}

// ---------------------------------------------------------------------------
// Controller load options
// ---------------------------------------------------------------------------

/**
 * Options for {@link EditorController.load}. The controller manages the
 * `beforeRun`, `afterRun`, and `onStreamItem` hooks internally; callers
 * supply everything else.
 */
export interface EditorLoadOptions {
  /** The graph, as an object or JSON text. */
  graph: GraphJson | string;
  /** Every component the graph instantiates. */
  components: ComponentSource[];
  /** The compiled `witgraph-web` WASM module. */
  wasm?: Wasm;
  /** Host-provided capabilities for node imports. */
  capabilities?: (node: string, capability: Capability) => object | undefined;
  /** Mock replacements for nodes, keyed by node id. */
  mocks?: Record<string, (inputs: PortValues) => PortValues | Promise<PortValues>>;
}
