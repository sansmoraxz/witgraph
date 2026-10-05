//! The witgraph scheduler for JavaScript hosts.
//!
//! This crate is `witgraph-sched` over an executor whose nodes are
//! JavaScript functions: each node's `run`, as `jco transpile` exposes it
//! (an object of inputs in, a promise of an object of outputs out). The
//! scheduling rules are the scheduler's, so a graph ticks, latches,
//! feeds back, faults and snapshots in a browser under the same rules as
//! on wasmtime. What differs is the engine's own business: there is no fuel
//! or memory limit, and a generation ends when every `run` has settled.
//! wasmtime also waits for work a guest keeps doing after its `run`
//! returns (a stream writer, say); a JavaScript host cannot see that work,
//! so a node's next `run` may start while its previous stream is still
//! being written. And each node is an instance of its own, so a failed call
//! is always pinned on its node, where wasmtime names no culprit for a
//! fault in a Store several members share.
//!
//! Built for `wasm32-unknown-unknown` and bound with `wasm-bindgen`, it
//! exports one class, `WebGraph`. On any other target the crate is empty.
//!
//! # Values
//!
//! A Value port's value is kept as a WAVE value of the port's type, so
//! values compare structurally and snapshots are the same WAVE text as on
//! wasmtime. They cross to and from JavaScript in jco's representation:
//! records are objects with `lowerCamelCase` keys, `option<T>` is the value
//! or `undefined` unless `T` is itself an option represented so, in which
//! case it is `{ tag: 'none' }` or `{ tag: 'some', val }` (the form
//! alternates with depth: `option<option<u32>>` is tagged,
//! `option<option<option<u32>>>` is not), `result` and variants are
//! `{ tag, val }`, enums are their case names, flags are objects of
//! booleans, 64-bit integers are `BigInt`s, and numeric lists are typed
//! arrays. A stream or future is whatever the node returned (an async
//! iterable, a thenable) and is handed to its reader untouched.

#[cfg(target_arch = "wasm32")]
mod executor;
#[cfg(target_arch = "wasm32")]
mod graph;
#[cfg(target_arch = "wasm32")]
mod live;
#[cfg(target_arch = "wasm32")]
mod value;

#[cfg(target_arch = "wasm32")]
pub use graph::{WebGraph, describe_component, js_name};
