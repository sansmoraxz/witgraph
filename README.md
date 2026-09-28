# witgraph
Easy and modular flow based composer

Graphs are built from WASM components whose public contracts are described in
[WIT](https://component-model.bytecodealliance.org/design/wit.html). WIT is the
source of truth: the typed graph IR, validation rules, and editor metadata are
all derived from it.

## Crates

- `witgraph-ir` — the typed graph IR: payload types, port kinds, component
  contracts, graph + builder, validation, and topology analysis. Kept light on
  dependencies (`serde` is an on-by-default feature) with future embedded
  targets in mind.
- `witgraph-wit` — loads `.wit` sources with `wit-parser`, lowers component
  worlds into IR contracts, computes content-hash identities, and generates
  the editor-facing metadata catalog.

## The `node` component convention

A witgraph component is a WIT **world** that exports an interface named
`node`. Ports are declared structurally through up to five well-known records
(at least one must be present); the world's **imported functions and
function-carrying interfaces are its capabilities** (type-only imports are
structural, not capabilities). A named interface's capability is its full id
(`namespace:name/interface@version`); an anonymous inline interface is scoped
to the importing world (`namespace:name/world.import-name@version`); bare
function imports are prefixed `func:`.

| Record           | Direction | Port kind | Field type meaning                          |
|------------------|-----------|-----------|---------------------------------------------|
| `inputs`         | input     | see below | `T` → Value; top-level `option<T>` → optional Value; `stream<T>` → Stream; `future<T>` → Future |
| `outputs`        | output    | see below | same mapping (no optional unwrapping)       |
| `input-events`   | input     | Event     | field type is the event payload directly    |
| `output-events`  | output    | Event     | field type is the event payload directly    |
| `drained-inputs` | input     | Stream/Future only | `stream<T>`/`future<T>` consumed to completion before the node's first activation, latched as the total |

The four port kinds:

- **Value** — latched, last-write-wins; read synchronously each iteration.
- **Event** — discrete occurrences, delivered asynchronously.
- **Stream** — ordered, back-pressured sequence.
- **Future** — one-shot asynchronous value.

```wit
package demo:graph@0.1.0;

world sensor {
    import demo:caps/clock@0.1.0;      // capability

    export node: interface {
        record reading { value: f64, timestamp: u64 }

        record inputs {
            sample-rate: option<u32>,  // optional Value input
        }
        record outputs {
            latest: reading,           // Value output
            samples: stream<reading>,  // Stream output
        }
        record output-events {
            threshold-crossed: f64,    // Event output (payload f64)
        }
    }
}
```

### Sync/async node coloring

A node's consumption mode is derived from its input ports, with one
exception-free rule: **any undrained Stream, Event, or Future input colors the
node async** — it fires per async activation, and its Value inputs act as
latched parameters sampled at each firing. A node with no undrained async
inputs is **sync**. Mixing kinds is legal. Outputs are unconstrained.

A **drained input** (declared in the `drained-inputs` record) is consumed to
completion — end-of-stream for a Stream, resolution for a Future — before the
node's first activation, then delivered once as a latched total. A node whose
async inputs are all drained is sync: it fires once with the drained totals.
Mixed drained + reactive inputs are legal too — the node activates only after
every drain completes, then fires per reactive activation.

Draining is per-port, part of the contract (and its content hash), and carries
two restrictions:

- **Stream and Future fields only** (enforced at lowering) — Values and Events
  have no completion semantics, so there is nothing to drain.
- **Drained connections stay off cycles** (enforced at graph compilation) — a
  connection feeding a drained input from within a cycle (feedback included)
  would deadlock waiting on its own downstream. Reactive ports on the same
  node may still close a loop.

### Connections, feedback, and compilation

Connections are legal only when port kinds are equal and payload types are
structurally equal — no coercion. Each input port accepts at most one writer;
outputs fan out freely. Cycles are legal only if every cycle crosses at least
one connection marked `feedback` (a unit-delay state boundary carrying the
previous iteration's value).

Compilation follows a typestate chain: `GraphBuilder → Graph → CompiledGraph`.
Only `Graph` is serializable; `Graph::compile(self)` validates the graph,
collecting every diagnostic rather than stopping at the first, and promotes to
`CompiledGraph`,
which exposes the topological order and aggregated capability requirements.

### Component identity

A component is identified by `namespace:name/world@version` plus a sha-256
content hash of its lowered contract. The hash is purely structural: it covers
the sorted ports and capabilities only — not the package, world, version, doc
comments, or type names — so any two contracts with the same shape hash
identically, and drained inputs are encoded with a distinct label so undrained
contracts keep their hashes. The canonical encoding carries its own version
line, so a deliberate format change shifts hashes explicitly.
