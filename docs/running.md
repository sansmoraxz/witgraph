# Running a graph

```rust
let mut rt = RuntimeGraph::load(compiled, &wasm, RuntimeConfig::default(), Perf).await?;
rt.inject(&"scale".into(), &"factor".into(), Val::U32(10)).await?;

for _ in 0..100 {
    match rt.tick_until(tokio::time::sleep(Duration::from_secs(1))).await {
        TickResult::Idle => break,
        TickResult::Aborted { fault, .. } => return Err(fault.into()),
        _ => {}
    }
    for report in rt.take_faults() {
        eprintln!("the graph faulted: {}", report.fault);
    }
}
let scaled = rt.read_output(&"scale".into(), &"out".into()).await?;
```

A graph runs as one composed component, in one wasmtime Store. A generated _hub_
holds every port's latest value in its own memory and moves values between nodes
itself, so they don't pass through the host. The host calls each node's `run`
through the hub, and sees a value only when it asks for one.

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
error if something doesn't fit, then generates the hub, composes it with the
nodes, and compiles the result. That is slow enough to matter, so on a shared
async executor prepare the graph first with `PreparedGraph::new` on a blocking
thread, then call `load_prepared`. Any change to the graph's nodes, connections
or links makes another composite, which is prepared again.

### Precompiled graphs

With the `precompiled` feature you can compile once, save the result, and skip
compiling on every later load:

```rust
// At build time.
let prepared = PreparedGraph::new(&engine, &compiled, &wasm)?;
std::fs::write("graph.bin", prepared.serialize()?)?;

// At startup, possibly in a build with no compiler.
let bytes = std::fs::read("graph.bin")?;
// SAFETY: we built this file, and only we can write to it.
let prepared = unsafe { PreparedGraph::deserialize(&engine, &compiled, &bytes) }?;
let rt = RuntimeGraph::load_prepared(compiled, &prepared, config, Perf, host).await?;
```

In a build that also has `compile`, `load_or_compile` does both: it loads the
file if it's still good, and otherwise compiles the bytes and replaces it.

```rust
// SAFETY: only we can write to this path.
let prepared = unsafe {
    PreparedGraph::load_or_compile(&engine, &compiled, &wasm, "graph.bin")
}?;
```

It also notices a component rebuilt with the same contract, or a changed
provider, because the file records a hash of what it was compiled from.
`deserialize` doesn't check that, since it isn't given the bytes.

Everything is composed in before saving, so `deserialize` doesn't need any
component's bytes. A file saved for another graph fails with `GraphMismatch`.

#### What a saved file depends on

**Warning: a saved file is native code for one exact build and machine.** It
isn't a portable format like a `.wasm`. To load, all of these must match the
build that made it:

- the exact wasmtime version, so `50.0.0` can't load a file from `50.0.1`
- the CPU architecture and operating system
- the compiler settings
- the CPU features it was compiled to use
- engine settings, such as `memory_reservation`, and enabled wasm features

A mismatch fails cleanly with `GraphMismatch`, or makes `load_or_compile`
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

Nodes importing one name share one import, so resource types unify across them.
Two nodes that want different providers of one interface use labels. A `Host`
links the union of what the graph imports, once, and is asked for one Store's
data at a time.

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
`Arc`), and it is dropped when the hub's slot, and any guest's own reference,
let go: its `Drop` frees what it holds. `RuntimeConfig::max_shared_memory`
(4 GiB by default) bounds the bytes plugins report for all of a graph's
objects; making one past it faults the graph.

`max_shared_memory` also counts what a stream of handles has in flight.
Backpressure holds a fast writer back, so an endless stream stays within a
small limit, but going over still faults the graph: it doesn't wait. Allow a
few objects per node in the stream.

## Ticks

Nothing runs between ticks. A tick runs every node that has new input until
nothing more can run, then advances each feedback loop by one iteration.
`tick_until` also stops when a timer or other future completes, so a graph with
an endless stream can't block you. Dropping a tick is safe: the next one picks
up where it left off.

A tick returns `Progress`, `Idle`, `Interrupted`, `StepLimitReached` or
`Aborted` (a node called `fatal`).

## Inputs and outputs

These are the methods of `RunningGraph`, from `witgraph-traits`, which every host
implements. `use witgraph_runtime::RunningGraph` to call them. Reads are never
pushed to you: you read an output when you choose.

- `inject(node, port, value)` sets a value input, and `inject_wave` does the
  same from text. Required inputs fed only by feedback need an initial value.
- `clear_input` removes an injected value.
- `read_output` returns the last value a node produced, `read_output_wave` the
  same as text, and `read_handle` the object behind a handle output. `reader()`
  gives you something that reads them while a tick is running too. They return
  `None` while the graph is stopped. `shared_memory()` is what the graph's
  shared objects take.
- `output_version` says which value of an output is latched, a number that moves
  on each time the node writes it, so you can tell it is new without reading it.
  It asks no guest anything.
- `rerun(node)` runs a node again with its current inputs.

A node only reruns when an input actually changes. The hub decides, comparing
values: every NaN is equal to every other, and `0.0` differs from `-0.0`. A node
returning a handle always counts as a change.

### Typed ports

When you know a port's type, bind it. The engine checks the type once, and a
call then does no name lookup and makes no `Val`, which is what you want for
anything large.

```rust
let scale = rt.input::<f64>(&"scale".into(), &"value".into()).await?;
let blob = rt.output::<Vec<u8>>(&"sink".into(), &"blob".into()).await?;
rt.set(&scale, 21.0).await?;
rt.tick().await;
let bytes: Option<Vec<u8>> = rt.get(&blob).await?;
```

Binding needs a live graph. A port rebinds itself after a rebuild, and using one
on another graph is an error. `handle_output` and `get_handle` do the same for a
handle output.

### Previews

A host that looks at an output now and then pays when it looks, and nothing
between looks. So check `output_version`, and read only when it moved:

```rust
if rt.output_version(&node, &port)? != seen {
    let preview = rt.preview(&node, &port).await?; // Preview::Bytes or Preview::Val
    let text = rt.preview_wave(&node, &port).await?;
}
```

`preview` reads a `list<u8>` as bytes with a typed call, and anything else as a
`Val`. A dynamic read past `RuntimeConfig::hostcall_fuel` faults the graph, as the
engine stops the Store: a `Val` costs about 48 bytes per byte of a list, so bind a
large port instead.

### Spinning guests

A read or write waits for the guest to suspend, because nothing else runs in the
Store until it does. With no fuel budget, a guest that spins makes that forever.
Bound it with `until`, which gives up with `RuntimeError::PullStopped`:

```rust
let value = witgraph_runtime::until(sleep(Duration::from_secs(1)), reader.read_output(&node, &port)).await;
```

A timeout around a tick still fires, because fuel yields to the host.

## Stream groups

A stream or future can be read once, so nodes joined by one always re-run
together: a _stream group_. If any member owes a run, they all do, and the group
starts as soon as one member can. Each member still runs when what it reads has
settled. Other nodes run on their own: they re-run only if an input changed.

A group starts only once every member's required inputs have values, so compiling
rejects one with a required input that is only written after the group has run.

When a group's runs have all returned its writers may still be going, so it can't
start again until no guest work is left anywhere in the graph. Beside an endless
stream, then, a stream group never re-runs: stop the graph to restart it.

`CompiledGraph::stream_groups()` lists them.

## Faults and stopping

A trap, running out of fuel or memory, or a `fatal` call stops the whole graph:
one composite is one Store, and a trap poisons it. `take_faults()` reports it, with
the culprit when it is known: the node that called `fatal`, or the only one with a
`run` in flight when no stream group was at work. `shutdown()` stops the graph too,
and forgets what was owed.

A stopped graph holds no values but the inputs you wrote. The next input change or
`rerun` rebuilds it: a fresh Store, your inputs written again, and every node run
again, with feedback loops starting over. Writing an equal value to a stopped graph
does nothing, unless it was written through a typed port, which can't be compared.

## Limits

`RuntimeConfig` sets limits for the graph, including a fuel budget per `run`
(`Fuel`), a memory cap (`max_memory`, a `ByteCount`), and a bound on how much work
one tick may do. The defaults are generous. All values share the hub's one 32-bit
memory, so a graph holds at most 4 GiB of them, and that memory never shrinks.

Nodes can also claim a fraction of a named resource. A node holds its claims while
it runs, and a stream group holds its members' from when it starts until it has
drained. The scheduler won't start what doesn't fit. Compiling rejects a group
claiming more than all of a resource, groups that wait on each other whose claims
can't fit together, and a group whose claims leave no room for a node only some of
its members wait on, since they would deadlock.

## Snapshots

`snapshot()` captures the graph's values, feedback state and what is in flight or
owed as serializable data, read through the hub as WAVE text. `restore(&snapshot)`
builds a new instance on a graph with the same nodes and writes them back. Guest
memory isn't captured: what was in flight re-runs from its start on the latched
inputs, so the result matches the original only if the nodes are deterministic.
A re-run also starts after any input written while the original was running, so
what it feeds back is the newest, where the original's would have been dropped.

Handles aren't captured either: the nodes that produced them are recorded as owing
a run, so after a restore they make new ones.

A snapshot of a stopped graph holds only the inputs you wrote, and fails on one
written through a typed port, which has no text. Snapshots from before stream
groups don't load.

## What one Store costs

Moving values inside one composite is what makes them cheap: a 16 MiB `list<u8>`
through source → relay → relay → sink takes about 113 ms, against 3,278 ms when
every hop went through the host. These follow from it:

| You might expect                                 | What happens                                                                                   |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| A trap to stop one node                          | It stops the whole graph.                                                                      |
| Values to stay readable after a fault            | They are gone until the rebuild's re-run. Deterministic nodes get the same ones back.          |
| A rebuild to cover what faulted                  | It re-runs every node, and resets feedback loops.                                              |
| To be told which node faulted                    | Only from `fatal`, or when exactly one `run` is in flight with no stream group at work.        |
| A spinning node to yield to the rest             | It blocks the graph, though a timeout around a tick still fires, as fuel yields to the host.   |
| `read_output` to answer at once                  | It waits for a spinning guest to suspend: bound it with `until`.                               |
| Stream groups to re-run independently            | One can't re-run until all guest work in the graph is done, so never beside an endless stream. |
| A group's members to read inputs from its start  | A node reads its inputs when it starts. A change after that owes another run.                  |
| A node returning the same handle to be no change | It always is a change: the hub can't compare handles.                                          |
| Memory to be unbounded                           | All values share one 32-bit memory: 4 GiB for the graph, which never shrinks.                  |
| A connection edit to be cheap                    | It regenerates, composes and compiles the graph: about 57 ms at 32 nodes (152 ms on jco).      |

## Cargo features

`witgraph-runtime` always enables what it needs from wasmtime: the component
model with async, and WAVE support. Two features decide how components get
loaded:

| Feature       | Default | What it does                                                                                     |
| ------------- | ------- | ------------------------------------------------------------------------------------------------ |
| `compile`     | yes     | Compiling with Cranelift, and composing a graph: `load`, `load_with_host`, `PreparedGraph::new`. |
| `precompiled` | no      | `PreparedGraph::serialize` and `deserialize`, plus `load_or_compile` with `compile`.             |

To drop the compiler from a build that only loads saved graphs, use
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

Most of these matter if you reach wasmtime through the
`witgraph_runtime::wasmtime` re-export, for example to build your own `Engine`
and pass it in `RuntimeConfig`. The engine witgraph builds itself uses only
three of them: `parallel-compilation`, which only makes compiling faster, and
`addr2line` and `demangle`, which change a trap's backtrace in its fault
message. With `addr2line` on, `WASMTIME_BACKTRACE_DETAILS=1` set and a guest
built with debug info, each frame gets its source location; an optimized guest
may resolve to a file with no line, so build it with `opt-level = 0` for the
exact one. `demangle` makes the frames' function names readable.

## Tracing

Pass `Trace` instead of `Perf` to record events such as runs starting and
returning, stream groups draining, faults and phase changes. Read them back with
`trace()` or drain them with `take_trace()`. Stream items pass between guests
unseen on wasmtime, so they aren't traced.
