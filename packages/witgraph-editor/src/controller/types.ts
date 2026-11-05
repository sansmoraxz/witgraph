import type { ComponentSource, PortValues, SessionWorker, Wasm } from 'witgraph-loader';

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
  kind: 'value' | 'stream' | 'future' | 'handle';
  typeDisplay: string;
  /** Handle ports only: the resource the port carries. */
  handle?: { interface: string; resource: string };
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
  /** What its component says of itself. */
  docs?: string;
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
  nodes?: string[];
  from?: string;
  to?: string;
  fault?: { kind: string; message: string };
  message?: string;
}

/** A fault report taken from the scheduler after a tick. */
export interface FaultReport {
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
   * future resolved to; `undefined` before there is one. For a stream or
   * future of handles, what the worker showed of it (`HandleView`, in `witgraph-loader`).
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
  /** How much of each named resource it holds while it runs. */
  resources?: Record<string, { amount: number }>;
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
  /** How much there is of a named resource. One not here has a capacity of 1. */
  resources?: Record<string, { capacity: number }>;
}

/** A named resource of a graph, as it is declared and claimed. */
export interface ResourceUse {
  name: string;
  /** How much there is of it. */
  capacity: number;
  /** Whether the graph says so, rather than it being the default. */
  declared: boolean;
  /** What its nodes claim, added up: more than the capacity is fine, as they take turns. */
  claimed: number;
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
  /**
   * Makes the worker sessions run in, which calls `serveSession` from `witgraph-loader`:
   * capabilities are provided there. The controller ends it (`terminate`) when it is
   * disposed, or when a node will not stop, and asks for another. A `MessagePort` will do,
   * whichever thread serves it, but with no `terminate` a node that spins cannot be ended.
   */
  worker: () => SessionWorker;
  /**
   * Lets a guest built with debug info be stopped on a line of its source. It needs a page
   * that is cross-origin isolated: elsewhere the graph runs, and the timeline says why not.
   */
  lines?: boolean | { all?: boolean; own?: readonly string[] };
  /** The text of a source file, by the path the debug info gives, for showing where it stops. */
  sources?: (file: string) => string | undefined | Promise<string | undefined>;
  /** Mock replacements for nodes, keyed by node id. */
  mocks?: Record<string, (inputs: PortValues) => PortValues | Promise<PortValues>>;
}
