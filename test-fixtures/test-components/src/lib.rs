//! Guest components used by the witgraph runtime tests.
//!
//! Every guest is a standalone crate under `guests/` whose `wit/` directory
//! is its contract. The build script compiles each one for
//! `wasm32-unknown-unknown` and encodes it as a component. Every node's
//! `run` is an `async func`, as witgraph requires.

/// One test guest: its component bytes and the directory holding its WIT
/// contract (lower it with `witgraph_wit::load_components`).
#[derive(Debug, Clone, Copy)]
pub struct Guest {
    /// The encoded WASM component.
    pub wasm: &'static [u8],
    /// The guest's `wit/` directory.
    pub wit: &'static str,
}

macro_rules! guest {
    ($wasm:literal, $dir:literal) => {
        Guest {
            wasm: include_bytes!(env!($wasm)),
            wit: concat!(env!("CARGO_MANIFEST_DIR"), "/guests/", $dir, "/wit"),
        }
    };
}

/// `in: option<f64>` → `out: f64`; absent input reads as 0.0.
pub const ECHO: Guest = guest!("ECHO_WASM", "echo");

/// `rate: option<u32>`, `gain: option<f64>` → `result: f64`
/// (`rate * gain`, defaults 1 and 1.0).
pub const CONFIGURABLE: Guest = guest!("CONFIGURABLE_WASM", "configurable");

/// `in: u32`, `add: option<u32>` → `out: u32` (`in + add`).
pub const RELAY: Guest = guest!("RELAY_WASM", "relay");

/// `burst-size: option<u32>` → `items: stream<u32>` streaming
/// 0, 1, 2, ... (forever when `burst-size` is absent), stopping once the
/// reader is dropped; `limit: u32` is `burst-size`, or `u32::MAX` when
/// endless, returned at once.
pub const STREAM_PRODUCER: Guest = guest!("STREAM_PRODUCER_WASM", "stream-producer");

/// `items: stream<u32>`, `take: option<u32>`, `delay: option<u32>` →
/// `total: u32`, `count: u32`. Returns once the stream ends or `take` items
/// were read; yields `delay` times per item.
pub const STREAM_CONSUMER: Guest = guest!("STREAM_CONSUMER_WASM", "stream-consumer");

/// `items: stream<u32>` → `items: stream<u32>`: returns the reader it was
/// given, so the items come straight from the upstream writer.
pub const STREAM_RELAY: Guest = guest!("STREAM_RELAY_WASM", "stream-relay");

/// `items: stream<u32>` → `started: bool` (always true): returns at once and
/// reads the stream to its end from a task it spawned.
pub const STREAM_DRAIN: Guest = guest!("STREAM_DRAIN_WASM", "stream-drain");

/// `again: option<u32>` → `done: future<u32>`, written 7 from a task after
/// `run` returned, and `last: u32`, how the previous run's write ended (0
/// none yet, 1 a reader took it, 2 the reader was dropped).
pub const FUTURE_WRITER: Guest = guest!("FUTURE_WRITER_WASM", "future-writer");

/// Imports the `test:mqtt/source` capability and streams its
/// messages on `messages: stream<u32>` until the feed is exhausted.
pub const MQTT_NODE: Guest = guest!("MQTT_NODE_WASM", "mqtt-node");

/// `action: option<enum { finish, spin, trap, fatal }>` (default
/// `finish`) → `done: u32`.
/// Spins forever, traps, or calls `witgraph:runtime/host.fatal` on request.
pub const BUSY_LOOP: Guest = guest!("BUSY_LOOP_WASM", "busy-loop");

/// `x: option<u32>` → `maybe: option<u32>`, passed straight through.
pub const MAYBE: Guest = guest!("MAYBE_WASM", "maybe");

/// The `relay` contract under `test:relay@0.2.0`, implemented
/// differently: `out = in + add + 1`. Its contract hashes like
/// [`RELAY`]'s.
pub const RELAY_PLUS: Guest = guest!("RELAY_PLUS_WASM", "relay-plus");

/// Another revision of `test:relay/relay@0.1.0` (the same id as
/// [`RELAY`], another content hash): adds `doubled: u32 = out * 2`.
pub const RELAY_WIDE: Guest = guest!("RELAY_WIDE_WASM", "relay-wide");

/// Exports `node` as the named interface `test:named/node@0.1.0`:
/// `in: option<f64>` → `out: f64 = in * 2` (absent reads as 0.0).
pub const NAMED_ECHO: Guest = guest!("NAMED_ECHO_WASM", "named-echo");

/// Imports `test:kv/store` under the labels `primary` and `backup`.
/// `key: option<string>` (default empty) → `primary: string`,
/// `backup: string`: what each store returns for the key.
pub const LABELLED: Guest = guest!("LABELLED_WASM", "labelled");

/// Not a node: a provider exporting `test:math/ops@0.1.0` (`double`,
/// `slow-double`, `count` and `calls`), for links.
pub const MATH: Guest = guest!("MATH_WASM", "math");

/// Imports `test:math/ops`. `x: option<u32>` (default 0) →
/// `doubled: u32`, `slow: u32` (both `x * 2`), `total: u32` (the sum of
/// `0..x`), `calls: u32` (how often its provider's `double` has run).
pub const CALC: Guest = guest!("CALC_WASM", "calc");

/// `burst-size: option<u32>` → `items: stream<reading>` (a record
/// `{ value: u32 }`), with values 0, 1, 2, ... (forever when `burst-size`
/// is absent). A stream of records cannot be moved between Stores.
pub const READING_PRODUCER: Guest = guest!("READING_PRODUCER_WASM", "reading-producer");

/// `items: stream<reading>` → `total: u32` (the sum of the values),
/// `count: u32`.
pub const READING_CONSUMER: Guest = guest!("READING_CONSUMER_WASM", "reading-consumer");
