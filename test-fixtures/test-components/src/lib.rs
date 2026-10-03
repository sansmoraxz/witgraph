//! Guest components used by the witgraph runtime tests.
//!
//! Every guest is a standalone crate under `guests/` whose `wit/` directory
//! is its contract. The build script compiles each one for
//! `wasm32-unknown-unknown` and encodes it as a component.

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

/// Sync. `in: option<f64>` → `out: f64`; absent input reads as 0.0.
pub const ECHO: Guest = guest!("ECHO_WASM", "echo");

/// Sync. `rate: option<u32>`, `gain: option<f64>` → `result: f64`
/// (`rate * gain`, defaults 1 and 1.0).
pub const CONFIGURABLE: Guest = guest!("CONFIGURABLE_WASM", "configurable");

/// Sync. `in: u32`, `add: option<u32>` → `out: u32` (`in + add`).
pub const RELAY: Guest = guest!("RELAY_WASM", "relay");

/// Async. `burst-size: option<u32>` → `items: stream<u32>` streaming
/// 0, 1, 2, ... (forever when `burst-size` is absent), stopping once the
/// reader is dropped; `limit: u32` is `burst-size`, or `u32::MAX` when
/// endless, returned at once.
pub const STREAM_PRODUCER: Guest = guest!("STREAM_PRODUCER_WASM", "stream-producer");

/// Async. `items: stream<u32>`, `take: option<u32>`, `delay: option<u32>` →
/// `total: u32`, `count: u32`. Returns once the stream ends or `take` items
/// were read; yields `delay` times per item.
pub const STREAM_CONSUMER: Guest = guest!("STREAM_CONSUMER_WASM", "stream-consumer");

/// Async. Imports the `test:mqtt/source` capability and streams its
/// messages on `messages: stream<u32>` until the feed is exhausted.
pub const MQTT_NODE: Guest = guest!("MQTT_NODE_WASM", "mqtt-node");

/// Async. `action: option<enum { finish, spin, trap, fatal }>` (default
/// `finish`) → `done: u32`.
/// Spins forever, traps, or calls `witgraph:runtime/host.fatal` on request.
pub const BUSY_LOOP: Guest = guest!("BUSY_LOOP_WASM", "busy-loop");

/// Sync. `x: option<u32>` → `maybe: option<u32>`, passed straight through.
pub const MAYBE: Guest = guest!("MAYBE_WASM", "maybe");

/// Sync. The `relay` contract under `test:relay@0.2.0`, implemented
/// differently: `out = in + add + 1`. Its contract hashes like
/// [`RELAY`]'s.
pub const RELAY_PLUS: Guest = guest!("RELAY_PLUS_WASM", "relay-plus");

/// Sync. Another revision of `test:relay/relay@0.1.0` (the same id as
/// [`RELAY`], another content hash): adds `doubled: u32 = out * 2`.
pub const RELAY_WIDE: Guest = guest!("RELAY_WIDE_WASM", "relay-wide");

/// Sync. Exports `node` as the named interface `test:named/node@0.1.0`:
/// `in: option<f64>` → `out: f64 = in * 2` (absent reads as 0.0).
pub const NAMED_ECHO: Guest = guest!("NAMED_ECHO_WASM", "named-echo");
