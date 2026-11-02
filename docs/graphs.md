# Graphs

A graph is plain, serializable data:

- the components it uses, each by name, version and content hash;
- nodes, each an instance of one of those components;
- connections from an output port to an input port;
- links, which supply a node's imports from another component.

A graph doesn't store contracts. Contracts always come from WIT.

## Getting contracts

`witgraph_wit::load_components(path)` reads a `.wit` file, a directory or an
encoded WIT package and returns a contract for each component world in it.
`lower_component(bytes, &id)` does the same from a compiled component.

## Compiling

```rust
let compiled = graph.compile(&contracts)?;
```

Compiling checks the graph against the contracts and reports every problem at
once, not just the first. On success you get a `CompiledGraph`, which you can
inspect (execution order, islands, required capabilities) and then run.

## Connection rules

- **Types must match exactly.** There is no coercion. The one exception is a
  value of type `option<T>` can feed an optional input of type `T`. Handles
  match when they carry the same resource of the same interface, at
  semver-compatible versions.
- **One writer per input.** An input has at most one incoming connection.
- **No fan-out for streams and futures.** Use a tee node to split one. Values
  and handles can fan out freely.
- **Required inputs must be connected.**
- **Cycles need a feedback edge.** Mark at least one connection in each cycle
  as `feedback`. It delivers the previous iteration's value and can only carry
  values and handles.

## Links

A capability normally comes from the host. A link supplies it from another
component instead:

```rust
let graph = Graph::builder("g")
    .add_component(&calc)
    .add_node("n", calc.id.clone())
    .link("l", "n", "test:math/ops@0.1.0", math_ref, "test:math/ops@0.1.0")
    .build();
```

The provider is an ordinary component. When the graph is loaded it is composed
into the node, so the node calls it directly. Each node gets its own provider
instance. This works the same in a browser or Node.js: the loader gets the
composed component and runs that. The `compose` feature of `witgraph-wit` does
the composition, and `witgraph-runtime` and `witgraph-web` turn it on.
Providers are listed in `components` like any other component, and are not
nodes.

## Editor catalog

`witgraph_wit::metadata::generate_catalog` produces a JSON catalog of
components, ports, capabilities and named types for graph editors.
