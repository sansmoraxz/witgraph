//! The witgraph scheduler for JavaScript hosts.
//!
//! This crate is `witgraph-sched` over a graph composed into one component
//! (`witgraph_wit::hub`), which `jco transpile` turns into JavaScript: the
//! scheduling rules are the scheduler's, and the hub is the one wasmtime runs, so a
//! graph ticks, latches, feeds back, faults and snapshots in a browser under the
//! same rules as on wasmtime. What differs is the engine's own business: there is
//! no fuel or memory limit. wasmtime also waits for work a guest keeps doing after
//! its `run` returns (a stream writer, say); a JavaScript host cannot see that
//! work, so a node's next `run` may start while its previous stream is still being
//! written. And what a failed call is pinned on is the node whose call failed.
//!
//! Built for `wasm32-unknown-unknown` (or, opting in, for
//! `wasm64-unknown-unknown`: a 64-bit memory, for values past what 4 GiB
//! holds) and bound with `wasm-bindgen`, it exports one class, `WebGraph`.
//! On any non-WebAssembly target the crate is empty.
//!
//! # Values
//!
//! A Value port's value is the hub's. Snapshots are the same WAVE text as on
//! wasmtime, and values cross to and from JavaScript in jco's representation:
//! records are objects with `lowerCamelCase` keys, `option<T>` is the value
//! or `undefined` unless `T` is itself an option represented so, in which
//! case it is `{ tag: 'none' }` or `{ tag: 'some', val }` (the form
//! alternates with depth: `option<option<u32>>` is tagged,
//! `option<option<option<u32>>>` is not), `result` and variants are
//! `{ tag, val }`, enums are their case names, flags are objects of
//! booleans, 64-bit integers are `BigInt`s, and numeric lists are typed
//! arrays. A stream or future is what jco hands the host (an async iterable, a
//! thenable), which the loader may watch on its way to its reader.

#[cfg(target_family = "wasm")]
mod graph;
#[cfg(target_family = "wasm")]
mod link;
#[cfg(target_family = "wasm")]
mod live;
#[cfg(target_family = "wasm")]
mod plane;
#[cfg(target_family = "wasm")]
mod value;

#[cfg(target_family = "wasm")]
pub use graph::{WebGraph, describe_component, js_name};
