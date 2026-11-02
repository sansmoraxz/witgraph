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

### Precompiled components

With the `precompiled` feature you can compile once, save the result, and skip
compiling on every later load:

```rust
// At build time.
let prepared = PreparedComponent::new(&engine, &contract, wasm)?;
std::fs::write("sensor.bin", prepared.serialize()?)?;

// At startup, possibly in a build with no compiler.
let bytes = std::fs::read("sensor.bin")?;
// SAFETY: we built this file, and only we can write to it.
let prepared = unsafe { PreparedComponent::deserialize(&engine, &contract, &bytes) }?;
let rt = RuntimeGraph::load_prepared(compiled, &[prepared], config, Perf, host).await?;
```

In a build that also has `compile`, `load_or_compile` does both: it loads the
file if it's still good, and otherwise compiles the bytes and replaces it.

```rust
// SAFETY: only we can write to this path.
let prepared = unsafe {
    PreparedComponent::load_or_compile(&engine, &contract, wasm, &[], "sensor.bin")
}?;
```

It also notices a component rebuilt with the same contract, or a changed link,
because the file records a hash of what it was compiled from. `deserialize`
doesn't check that, since it isn't given the bytes.

Links are composed in before saving, so `deserialize` doesn't need the
providers' bytes. A different contract fails with `ContractMismatch`.

#### What a saved file depends on

**Warning: a saved file is native code for one exact build and machine.** It
isn't a portable format like a `.wasm`. To load, all of these must match the
build that made it:

- the exact wasmtime version, so `50.0.0` can't load a file from `50.0.1`
- the CPU architecture and operating system
- the compiler settings
- the CPU features it was compiled to use
- engine settings, such as `memory_reservation`, and enabled wasm features

A mismatch fails cleanly with `BadComponent`, or makes `load_or_compile`
recompile. So:

- Keep the `.wasm` as your source, and treat saved files as build output.
- Build them in the same CI job and from the same `Cargo.lock` as the binary
  that loads them. A `cargo update` can make every file stale.
- Build on the oldest CPU you deploy to. By default the compiler uses every
  feature of the CPU it runs on.
- A build with no compiler can't recover from a stale file. You have to deploy
  matching files.

**These checks catch mistakes, not tampering.** A modified file can pass them
and then run as native code, which is why `deserialize` and `load_or_compile`
are `unsafe`. Treat a saved file like an executable: load only files you built,
from storage only you can write to.

## Giving nodes capabilities

Nodes that import capabilities load with `load_with_host`. The simplest host is
`Plugins`, built from plugins that each provide one or more capabilities:

```rust
let host = Plugins::new().with(Clock)?.with(Store("primary"))?.with(Store("backup"))?;
let rt = RuntimeGraph::load_with_host(compiled, &wasm, config, Perf, host).await?;
```

If a node imports a capability that no plugin provides, loading fails. If
several plugins provide it, the node's import label picks one.

## Sharing resources

A resource handle ports carry ([Handles](components.md#handles)) is one a
plugin shares. The plugin:

- says so in `CapabilityPlugin::shares` (a `Host` in `Host::shares`). A graph
  with a handle port carrying a resource nothing shares fails to load with
  `LoadError::NotShareable`;
- defines the resource with `shared::link_resource`;
- makes objects with `HostState::share(interface, resource, object, bytes)`,
  which returns the guest's `own` handle, and finds the object behind a
  handle its methods get with `HostState::shared`. `share_again` gives a new
  handle to an existing object, as a `clone` would.

```rust
let mut instance = linker.instance("test:data/tensor@0.1.0")?;
shared::link_resource(&mut instance, "test:data/tensor@0.1.0", "tensor")?;
instance.func_wrap("[method]tensor.clone", |mut store, (this,): (ResourceDynamic,)| {
    let state = store.data_mut().host_state();
    let tensor = state.shared(&this)?;
    Ok((state.share_again(tensor)?,))
})?;
```

The object is whatever the plugin keeps (a `Shared` holds it, behind an
`Arc`), and it is dropped when the last latch, or a guest's own reference,
lets go: its `Drop` frees what it holds. `RuntimeConfig::max_shared_memory`
(4 GiB by default) bounds the bytes plugins report for all of a graph's
objects; making one past it faults the node making it.

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
- `read_output` returns the last value a node produced, and `read_handle`
  the object behind a handle output. `shared_memory()` is what the graph's
  shared objects take.
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

Handles aren't captured either: the islands that produced them are recorded as
owing a run, so after a restore they make new ones.

## Choosing an allocator

witgraph uses your program's global allocator. The choice matters for values
made of many small pieces, such as a `list<record>` with one record per pixel,
because every hop allocates several times per item. A list of numbers, or a
record holding one, is packed and barely notices.

Time for one run of source → relay → relay → sink on Linux:

| Value                  | Size   | glibc    | mimalloc |
| ---------------------- | ------ | -------- | -------- |
| list of 4-byte records | 1 MiB  | 1,129 ms | 405 ms   |
| list of 4-byte records | 4 MiB  | 4,657 ms | 1,628 ms |
| `list<u8>`             | 1 MiB  | 205 ms   | 189 ms   |
| `list<u8>`             | 16 MiB | 3,181 ms | 3,230 ms |

On glibc, such a graph also slows down over its first dozen or so runs, then
levels off: from about 870 ms to 1,600 ms at 1 MiB. Memory use stays flat, so
this isn't a leak. It doesn't happen on mimalloc.

To switch, set the allocator in your binary:

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

For bulk data, a list of numbers or a [handle](#sharing-resources) still beats
any allocator.

## Cargo features

`witgraph-runtime` always enables what it needs from wasmtime: the component
model with async, and WAVE support. Two features decide how components get
loaded:

| Feature       | Default | What it does                                                                                                  |
| ------------- | ------- | ------------------------------------------------------------------------------------------------------------- |
| `compile`     | yes     | Compiling components with Cranelift, and composing links: `load`, `load_with_host`, `PreparedComponent::new`. |
| `precompiled` | no      | `PreparedComponent::serialize` and `deserialize`, plus `load_or_compile` with `compile`.                      |

To drop the compiler from a build that only loads saved components, use
`default-features = false, features = ["precompiled"]`.

The rest is opt-in too, so you only compile what you use:

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
