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

/// `x: option<u32>` → `out: u32`, adding what a capability whose import is named
/// `m0`, like the import of a mocked node, returns.
pub const M0_IMPORT: Guest = guest!("M0_IMPORT_WASM", "m0-import");

/// `rate: option<u32>`, `gain: option<f64>` → `result: f64`
/// (`rate * gain`, defaults 1 and 1.0).
/// Previews `result` as `series`.
pub const CONFIGURABLE: Guest = guest!("CONFIGURABLE_WASM", "configurable");

/// `in: u32`, `add: option<u32>` → `out: u32` (`in + add`).
/// Previews `out` as `series`.
pub const RELAY: Guest = guest!("RELAY_WASM", "relay");

/// `burst-size: option<u32>` → `items: stream<u32>` streaming
/// 0, 1, 2, ... (forever when `burst-size` is absent), stopping once the
/// reader is dropped; `limit: u32` is `burst-size`, or `u32::MAX` when
/// endless, returned at once.
/// Previews `items` as `series`.
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
/// Previews `done` as `text`.
pub const FUTURE_WRITER: Guest = guest!("FUTURE_WRITER_WASM", "future-writer");

/// `data: future<u32>` → `value: u32`, the value written by the future writer.
pub const FUTURE_READER: Guest = guest!("FUTURE_READER_WASM", "future-reader");

/// Imports the `test:mqtt/source` capability and streams its
/// messages on `messages: stream<list<u8>>` until the feed is exhausted.
/// Previews `messages` as `text`.
pub const MQTT_NODE: Guest = guest!("MQTT_NODE_WASM", "mqtt-node");

/// `items: stream<list<u8>>` → `count: u32`, `total-bytes: u32`.
/// Counts messages and sums their lengths.
pub const BYTES_CONSUMER: Guest = guest!("BYTES_CONSUMER_WASM", "bytes-consumer");

/// `action: option<enum { finish, spin, trap, fatal, spin-a-while }>` (default
/// `finish`) → `done: u32`.
/// Spins forever or for a while, traps, or calls `witgraph:runtime/host.fatal` on
/// request.
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

/// Imports `wasi:keyvalue/store` under the labels `primary` and `backup`.
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

/// Imports `wasi:clocks/monotonic-clock` (`wait-for`).
/// `frames: option<u32>`, `fps: option<u32>` → `frames: stream<frame>`
/// (a record `{ width, height, pixels: list<u8> }`, 96x64 RGBA): an
/// animated gradient, `frames` of them (forever when absent), sleeping
/// `1000 / fps` ms between two when `fps` is given. Its `witgraph-viewer`
/// section previews `frames` as `image`.
pub const VIDEO_SOURCE: Guest = guest!("VIDEO_SOURCE_WASM", "video-source");

/// `frames: stream<frame>` → `frames: stream<frame>`, each frame turned
/// gray (red, green and blue set to the luma). Previews `frames` as `image`.
pub const FRAME_GRAYSCALE: Guest = guest!("FRAME_GRAYSCALE_WASM", "frame-grayscale");

/// `frames: stream<frame>` → `frames: stream<frame>`, each frame's colours
/// inverted (alpha kept). Previews `frames` as `image`.
pub const FRAME_INVERT: Guest = guest!("FRAME_INVERT_WASM", "frame-invert");

/// `frames: stream<frame>` → `count: u32`, `gray: bool` (every pixel had
/// equal red, green and blue), `first-pixel: u32` (the last frame's first
/// pixel as RGBA, high byte first). Returns once the stream ends.
pub const VIDEO_SINK: Guest = guest!("VIDEO_SINK_WASM", "video-sink");

/// Imports `test:accel/cnn` (a small convolutional network, run on the
/// host's accelerator) and `witgraph:runtime/host`. `frames: stream<frame>`
/// → `edges: stream<frame>` (each frame's edges, by a fixed-weight
/// network of Sobel kernels), `backend: string` (the accelerator's name).
/// Faults, saying why, when the host has no accelerator. Previews `edges`
/// as `image`.
pub const EDGE_DETECT: Guest = guest!("EDGE_DETECT_WASM", "edge-detect");

/// Imports `wasi:webgpu/webgpu` and `witgraph:runtime/host`. The same network
/// as `EDGE_DETECT`, as compute shaders of its own on the host's GPU: `frames:
/// stream<frame>` → `edges: stream<frame>`, `backend: string` (`webgpu`).
/// Faults, saying why, when the host has no GPU. Previews `edges` as `image`.
pub const EDGE_DETECT_WEBGPU: Guest = guest!("EDGE_DETECT_WEBGPU_WASM", "edge-detect-webgpu");

/// `size: option<u32>`, `seed: option<u32>` (both default 0) →
/// `blob: list<u8>`, `size` bytes of `(i + seed) % 256`. For measuring how
/// data moves.
pub const BLOB_SOURCE: Guest = guest!("BLOB_SOURCE_WASM", "blob-source");

/// `blob: list<u8>` → `blob: list<u8>`, unchanged. For measuring how data
/// moves.
pub const BLOB_RELAY: Guest = guest!("BLOB_RELAY_WASM", "blob-relay");

/// `blob: list<u8>` → `len: u32`, `last: u8` (0 for none). For measuring how
/// data moves.
pub const BLOB_SINK: Guest = guest!("BLOB_SINK_WASM", "blob-sink");

/// Imports `test:data/tensor` (a tensor the host keeps) and
/// `witgraph:runtime/host`, calling `fatal` when no tensor can be made. `width`,
/// `height`, `seed`: `option<u32>` (64, 48 and 0 when absent) → `tensor:
/// own<tensor>`, a `[height, width, 3]` `uint8` gradient with a square in
/// the middle. Previews `tensor`
/// as `tensor`.
pub const TENSOR_SOURCE: Guest = guest!("TENSOR_SOURCE_WASM", "tensor-source");

/// Imports `test:data/tensor`. `tensor: borrow<tensor>` → `tensor:
/// own<tensor>`, inverted by the host (`invert`): no byte enters the node.
/// Previews `tensor` as `tensor`.
pub const TENSOR_INVERT: Guest = guest!("TENSOR_INVERT_WASM", "tensor-invert");

/// Imports `test:data/tensor`. `tensor: borrow<tensor>` (`uint8`) → `tensor:
/// own<tensor>`, inverted by the node itself: its bytes copied in and out.
/// Previews `tensor` as `tensor`.
pub const TENSOR_COPY_INVERT: Guest = guest!("TENSOR_COPY_INVERT_WASM", "tensor-copy-invert");

/// Imports `test:data/tensor`. `tensor: borrow<tensor>` → `sum: u64` (of
/// its bytes), `len: u64` (how many), read 64 KiB at a time.
pub const TENSOR_SINK: Guest = guest!("TENSOR_SINK_WASM", "tensor-sink");

/// Imports `test:data/tensor`. `tensor: option<borrow<tensor>>` →
/// `previous: u64`: keeps its own reference (`clone`) to each input until
/// the next, and returns the sum of the bytes of the one before (0 for
/// none).
pub const TENSOR_KEEP: Guest = guest!("TENSOR_KEEP_WASM", "tensor-keep");

/// Imports `test:data/tensor`. `tensor: borrow<tensor>` (`uint8`) →
/// `edges: own<tensor>`, by a fixed 3x3 Laplacian the host convolves
/// (`conv3x3`). Calls `fatal` with the host's reason when it cannot. Previews
/// `edges` as `tensor`.
pub const TENSOR_EDGE: Guest = guest!("TENSOR_EDGE_WASM", "tensor-edge");

/// `data: future<u32>`, dropped unread → `done: bool`.
pub const FUTURE_DROPPER: Guest = guest!("FUTURE_DROPPER_WASM", "future-dropper");

/// Imports `test:data/tensor` and `witgraph:runtime/host`. `frames`, `width`,
/// `height`: `option<u32>`, `reuse`: `option<bool>` → `tensors: stream<tensor>`,
/// `frames` tensors (forever when absent), 16 x 16 when not given. With `reuse`,
/// it makes one and hands out references to it, as from a camera the host owns.
/// Previews `tensors` as `tensor`.
pub const TENSOR_STREAM_SOURCE: Guest = guest!("TENSOR_STREAM_SOURCE_WASM", "tensor-stream-source");

/// Imports `test:data/tensor`. `tensors: stream<tensor>` → `tensors:
/// stream<tensor>`, each inverted by the host. Previews `tensors` as `tensor`.
pub const TENSOR_STREAM_INVERT: Guest = guest!("TENSOR_STREAM_INVERT_WASM", "tensor-stream-invert");

/// Imports `test:data/tensor` and `witgraph:runtime/host`. `tensors:
/// stream<tensor>` → `tensors: stream<tensor>`, the edges of each, found by the
/// host. Previews `tensors` as `tensor`.
pub const TENSOR_STREAM_EDGE: Guest = guest!("TENSOR_STREAM_EDGE_WASM", "tensor-stream-edge");

/// Imports `test:camera/camera`, `test:data/tensor` and `witgraph:runtime/host`.
/// `frames`: `option<u32>` → `tensors: stream<tensor>`, the host's camera, whose
/// pixels never enter the node. Previews `tensors` as `tensor`.
pub const CAMERA_SOURCE: Guest = guest!("CAMERA_SOURCE_WASM", "camera-source");

/// Imports `test:data/tensor`. `tensors: stream<tensor>`, `take`: `option<u32>`,
/// `read-last`: `option<bool>` → `count: u32`, `sum: u64`: reads `take` tensors
/// (all when absent), summing each one's first byte, or only the last's.
pub const TENSOR_STREAM_SINK: Guest = guest!("TENSOR_STREAM_SINK_WASM", "tensor-stream-sink");

/// Imports `test:data/tensor` and `witgraph:runtime/host`. `seed`:
/// `option<u32>` → `tensor: future<tensor>`, filled with `seed`.
pub const TENSOR_FUTURE_SOURCE: Guest = guest!("TENSOR_FUTURE_SOURCE_WASM", "tensor-future-source");

/// Imports `test:data/tensor`. `tensor: future<tensor>`, `ignore`: `option<bool>`
/// → `first: u32`, its first byte, or 1000 when told to drop the future unread.
pub const TENSOR_FUTURE_SINK: Guest = guest!("TENSOR_FUTURE_SINK_WASM", "tensor-future-sink");

/// `value`, `other` passed through unchanged: a value of every kind a port can
/// hold, in a record too big to be passed flat.
pub const CORPUS_RELAY: Guest = guest!("CORPUS_RELAY_WASM", "corpus-relay");

/// `mixed` passed through unchanged: a variant whose cases share core
/// values of different types, in a record small enough to be passed flat.
pub const CORPUS_FLAT: Guest = guest!("CORPUS_FLAT_WASM", "corpus-flat");

/// A guest built with debug info and no optimizing, for stepping through its source.
pub const LINE_WALK: Guest = guest!("LINE_WALK_WASM", "line-walk");
