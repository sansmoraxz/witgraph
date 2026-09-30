/// Compiled echo node WASM component.
///
/// Ports: optional Value input `in` (f64) → Value output `out` (f64).
/// Reads `in`, writes it to `out`. Defaults to 0.0 when absent.
pub const ECHO: &[u8] = include_bytes!(env!("ECHO_WASM"));

/// Compiled configurable node WASM component.
///
/// Ports: optional Value inputs `rate` (u32) + `gain` (f64) →
/// Value output `result` (f64). Writes `rate as f64 * gain`.
/// Defaults to 1 and 1.0 when absent.
pub const CONFIGURABLE: &[u8] = include_bytes!(env!("CONFIGURABLE_WASM"));

/// Compiled stream-producer WASM component.
///
/// Ports: optional Value input `burst-size` (u32, default 1),
/// Stream output `items` (u32). Pushes `burst-size` items then
/// closes the stream.
pub const STREAM_PRODUCER: &[u8] = include_bytes!(env!("STREAM_PRODUCER_WASM"));

/// Compiled stream-consumer WASM component.
///
/// Ports: Stream input `items` (u32), Value output `total` (u32).
/// Accumulates stream items into a running sum.
pub const STREAM_CONSUMER: &[u8] = include_bytes!(env!("STREAM_CONSUMER_WASM"));

/// Compiled MQTT node WASM component with custom host import.
///
/// Ports: Value output `count` (u32). Imports
/// `witgraph:runtime/mqtt-source::next-message` and counts messages.
pub const MQTT_NODE: &[u8] = include_bytes!(env!("MQTT_NODE_WASM"));
