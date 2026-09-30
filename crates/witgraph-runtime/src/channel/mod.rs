//! Port interconnection channels.
//!
//! Each connection in a compiled graph is backed by a channel whose kind
//! matches the port's [`PortKind`]:
//!
//! | Port kind | Channel type |
//! |-----------|--------------|
//! | `Value`   | [`ValueSlot`] |
//! | `Event`   | [`EventQueue`] |
//! | `Stream`  | [`StreamChannel`] |
//! | `Future`  | [`FutureSlot`] |

pub mod event;
pub mod future;
pub mod stream;
pub mod value;

pub use event::EventQueue;
pub use future::FutureSlot;
pub use stream::{StreamChannel, StreamPull};
pub use value::ValueSlot;

use witgraph_ir::PortKind;

/// A channel backing one connection in the runtime graph.
///
/// The variant is determined by the port kind of the connection's
/// endpoints at construction time.
#[derive(Debug, Clone)]
pub enum Channel {
    /// A latched last-write-wins value.
    Value(ValueSlot),
    /// A bounded FIFO event queue.
    Event(EventQueue),
    /// A bounded FIFO stream with end-of-stream signaling.
    Stream(StreamChannel),
    /// A one-shot future resolution.
    Future(FutureSlot),
}

impl Channel {
    /// Creates a channel matching the given port kind.
    ///
    /// `capacity` is used for bounded channels (Event, Stream); Value
    /// and Future channels ignore it.
    pub fn for_kind(kind: PortKind, capacity: usize) -> Self {
        match kind {
            PortKind::Value => Channel::Value(ValueSlot::new()),
            PortKind::Event => Channel::Event(EventQueue::new(capacity)),
            PortKind::Stream => Channel::Stream(StreamChannel::new(capacity)),
            PortKind::Future => Channel::Future(FutureSlot::new()),
        }
    }
}
