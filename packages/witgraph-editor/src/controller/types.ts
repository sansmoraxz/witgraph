import type { Capability, ComponentSource, PortValues, Wasm } from 'witgraph-loader';

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
  /** Output ports only: the viewer the component declares for previewing it. */
  viewer?: string;
}

/** The preview region of a node: one output, shown with one viewer. */
export interface NodePreview {
  /** The output previewed. */
  port: string;
  /** The viewer showing it (see `registerViewer`). */
  viewer: string;
  /** The region's height, below the ports band. */
  height: number;
}

/** A node on the canvas, combining layout, contract, and runtime state. */
export interface NodeVisual {
  id: string;
  x: number;
  y: number;
  width: number;
  /** The whole node's: header, ports band and preview region. */
  height: number;
  /** The ports band's, below the header. */
  portsHeight: number;
  /** The node's preview region, when it has one. */
  preview?: NodePreview;
  label: string;
  phase: NodePhase;
  fault?: { kind: string; message: string };
  culprit?: string;
  breakpoint: boolean;
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

/**
 * What an output has carried, as a preview shows it. Values are as jco
 * represents them (records as objects, `list<u8>` as `Uint8Array`, 64-bit
 * integers as `BigInt`s).
 */
export interface OutputView {
  kind: PortVisual['kind'];
  /**
   * A Value output's current value, the last item of a stream, or what a
   * future resolved to; `undefined` before there is one.
   */
  latest: unknown;
  /**
   * The values before it, oldest first and `latest` last: one per tick for
   * a Value output, one per item for a stream. A bounded window.
   */
  recent: readonly unknown[];
  /**
   * Ticks a Value output had a value at, items a stream carried, futures
   * that resolved: counted since the graph was loaded.
   */
  count: number;
  /** A future only: its node ran again, and the new one has not resolved yet. */
  pending: boolean;
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
// Capabilities and mocks
// ---------------------------------------------------------------------------

/** Info about one capability a node's component imports, with satisfaction status. */
export interface CapabilityInfo {
  /** Full interface id, e.g. `test:math/ops@0.1.0`. */
  interface: string;
  /** For a labelled import, the interface the label stands for. */
  implements?: string;
  /** Whether the host provides an implementation for this capability. */
  satisfied: boolean;
}

/** A saved simulation snapshot. */
export interface SavedSnapshot {
  name: string;
  data: string;
  timestamp: number;
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
