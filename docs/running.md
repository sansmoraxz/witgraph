# Running a graph

```rust
let mut rt = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf).await?;
rt.inject(&"scale".into(), &"factor".into(), Val::U32(10))?;

for _ in 0..100 {
    match rt.tick_until(tokio::time::sleep(Duration::from_secs(1))).await {
        TickResult::Idle => break,
        TickResult::Aborted { fault, .. } => return Err(fault.into()),
        _ => {}
    }
    for report in rt.take_faults() {
        eprintln!("{:?} faulted: {}", report.members, report.fault);
    }
}
let scaled = rt.read_output(&"scale".into(), &"out".into())?;
```

## Loading

`RuntimeGraph::load` takes the compiled graph and a map from each component to
its bytes. The bytes can come from anywhere. `witgraph-source` fetches them
from files or OCI registries, optionally pinned to a digest and cached:

```rust
let sources = vec![
    ComponentSource::oci(sensor, "ghcr.io/demo/sensor:0.1.0").pinned(digest),
    ComponentSource::file(alarm, "components/alarm.wasm"),
];
let components = Resolver::new().base(dir).cache(cache_dir).resolve(&sources).await?;
let rt = RuntimeGraph::load(compiled, &components.wasm(), config, Perf).await?;
```

Loading checks each component against its contract and fails with a named
error if something doesn't fit. Compiling components is slow, so on a shared
async executor prepare them first with `PreparedComponent::new` on a blocking
thread, then call `load_prepared`.

## Giving nodes capabilities

Nodes that import capabilities load with `load_with_host`. The simplest host is
`Plugins`, built from plugins that each provide one or more capabilities:

```rust
let host = Plugins::new().with(Clock)?.with(Store("primary"))?.with(Store("backup"))?;
let rt = RuntimeGraph::load_with_host(compiled, &wasm, config, Perf, host).await?;
```

If a node imports a capability that no plugin provides, loading fails. If
several plugins provide it, the node's import label picks one.

## Ticks

Nothing runs between ticks. A tick runs every node that has new input until
nothing more can run, then advances each feedback loop by one iteration.
`tick_until` also stops when a timer or other future completes, so a graph with
an endless stream can't block you. Dropping a tick is safe: the next one picks
up where it left off.

A tick returns `Progress`, `Idle`, `Interrupted`, `StepLimitReached` or
`Aborted` (a node called `fatal`).

## Inputs and outputs

- `inject(node, port, value)` sets a value input, and `inject_wave` does the
  same from text. Required inputs fed only by feedback need an initial value.
- `clear_input` removes an injected value.
- `read_output` returns the last value a node produced.
- `rerun(node)` runs a node again with its current inputs.

A node only reruns when an input actually changes.

## Islands

Nodes joined by streams or futures run together as an _island_, which is the
unit that runs, fails and restarts. Nodes with no stream or future connections
are islands of one.

## Faults and cancelling

If a node traps, runs out of fuel or memory, or calls `fatal`, its whole
island stops. Other islands keep running. `take_faults()` lists what failed.
`cancel(node)` stops a node's island, and `shutdown()` stops everything. A
stopped island is rebuilt the next time its input changes or you call `rerun`.

## Limits

`RuntimeConfig` sets limits for each island, including a fuel budget per `run`,
a memory cap, and a bound on how much work one tick may do. The defaults are
generous. Nodes can also claim a fraction of a named resource, and the
scheduler won't run islands together if their claims add up to more than the
resource has.

## Snapshots

`snapshot()` captures the graph's values, feedback state and any work in
flight as serializable data. `restore(&snapshot)` puts it back on a graph with
the same nodes. Guest memory isn't captured: work that was in flight is rerun
from its inputs, so the result matches the original only if the nodes are
deterministic.

## Cargo features

`witgraph-runtime` always enables what it needs from wasmtime: the Cranelift
compiler, the component model with async, and WAVE support. Everything else is
opt-in, so you only compile what you use:

| Feature                | Default | What it turns on in wasmtime                         |
| ---------------------- | ------- | ---------------------------------------------------- |
| `parallel-compilation` | yes     | Compiles a component's functions on several threads. |
| `cache`                | no      | The on-disk cache of compiled components.            |
| `pooling-allocator`    | no      | The pooling instance allocator.                      |
| `profiling`            | no      | Profiler support.                                    |
| `wat`                  | no      | Accepting text-format `.wat` input.                  |
| `coredump`             | no      | Guest core dumps on a trap.                          |
| `debug`                | no      | Guest debugging support.                             |
| `demangle`             | no      | Demangled names in backtraces.                       |
| `addr2line`            | no      | Source locations in backtraces.                      |

These matter if you reach wasmtime through the `witgraph_runtime::wasmtime`
re-export, for example to build your own `Engine` and pass it in
`RuntimeConfig`. The engine witgraph builds itself uses none of them, apart
from `parallel-compilation`, which only makes compiling faster.

## Tracing

Pass `Trace` instead of `Perf` to record events such as runs starting and
returning, faults and phase changes. Read them back with `trace()` or drain
them with `take_trace()`.
