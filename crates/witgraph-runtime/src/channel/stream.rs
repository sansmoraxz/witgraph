//! Bounded FIFO stream channel with end-of-stream signaling.

use std::collections::VecDeque;

use crate::error::ChannelError;
use witgraph_ir::Val;

/// The result of pulling from a stream channel.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamPull {
    /// An item was available.
    Item(Val),
    /// No items are available but the stream is still open.
    Pending,
    /// The stream has been closed (end-of-stream) and is empty.
    Closed,
}

/// A bounded FIFO stream channel with end-of-stream (EOS) signaling.
///
/// Items are pushed by producers and pulled by consumers. The channel
/// has a configurable capacity. Once closed, no further pushes are
/// accepted; pulls drain any remaining buffered items and then report
/// `Closed`.
#[derive(Debug, Clone)]
pub struct StreamChannel {
    buffer: VecDeque<Val>,
    capacity: usize,
    closed: bool,
}

impl StreamChannel {
    /// Creates an open, empty stream with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
            closed: false,
        }
    }

    /// Pushes an item. Returns [`ChannelError::Closed`] if the stream
    /// has been closed, or [`ChannelError::Full`] if the buffer is at
    /// capacity.
    pub fn push(&mut self, val: Val) -> Result<(), ChannelError> {
        if self.closed {
            return Err(ChannelError::Closed);
        }
        if self.buffer.len() >= self.capacity {
            return Err(ChannelError::Full(self.capacity));
        }
        self.buffer.push_back(val);
        Ok(())
    }

    /// Pulls the next item, reports pending if empty and open, or
    /// closed if empty and closed.
    pub fn pull(&mut self) -> StreamPull {
        if let Some(val) = self.buffer.pop_front() {
            return StreamPull::Item(val);
        }
        if self.closed {
            StreamPull::Closed
        } else {
            StreamPull::Pending
        }
    }

    /// Closes the stream. Returns [`ChannelError::Closed`] if already
    /// closed.
    pub fn close(&mut self) -> Result<(), ChannelError> {
        if self.closed {
            return Err(ChannelError::Closed);
        }
        self.closed = true;
        Ok(())
    }

    /// Returns `true` if the stream has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Returns `true` if no items are buffered.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// The number of items currently buffered.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pull_ordering() {
        let mut ch = StreamChannel::new(8);
        ch.push(Val::U32(1)).unwrap();
        ch.push(Val::U32(2)).unwrap();
        assert_eq!(ch.pull(), StreamPull::Item(Val::U32(1)));
        assert_eq!(ch.pull(), StreamPull::Item(Val::U32(2)));
        assert_eq!(ch.pull(), StreamPull::Pending);
    }

    #[test]
    fn close_drains_then_signals() {
        let mut ch = StreamChannel::new(8);
        ch.push(Val::U32(10)).unwrap();
        ch.close().unwrap();
        assert_eq!(ch.pull(), StreamPull::Item(Val::U32(10)));
        assert_eq!(ch.pull(), StreamPull::Closed);
    }

    #[test]
    fn push_after_close_rejected() {
        let mut ch = StreamChannel::new(8);
        ch.close().unwrap();
        let err = ch.push(Val::U32(1)).unwrap_err();
        assert!(matches!(err, ChannelError::Closed));
    }

    #[test]
    fn double_close_rejected() {
        let mut ch = StreamChannel::new(8);
        ch.close().unwrap();
        let err = ch.close().unwrap_err();
        assert!(matches!(err, ChannelError::Closed));
    }

    #[test]
    fn capacity_limit() {
        let mut ch = StreamChannel::new(2);
        ch.push(Val::U8(1)).unwrap();
        ch.push(Val::U8(2)).unwrap();
        let err = ch.push(Val::U8(3)).unwrap_err();
        assert!(matches!(err, ChannelError::Full(2)));
    }

    #[test]
    fn pull_frees_capacity() {
        let mut ch = StreamChannel::new(1);
        ch.push(Val::U8(1)).unwrap();
        assert!(ch.push(Val::U8(2)).is_err());
        ch.pull();
        ch.push(Val::U8(3)).unwrap();
        assert_eq!(ch.pull(), StreamPull::Item(Val::U8(3)));
    }

    #[test]
    fn empty_open_stream_is_pending() {
        let mut ch = StreamChannel::new(4);
        assert_eq!(ch.pull(), StreamPull::Pending);
    }

    #[test]
    fn empty_closed_stream_is_closed() {
        let mut ch = StreamChannel::new(4);
        ch.close().unwrap();
        assert_eq!(ch.pull(), StreamPull::Closed);
    }
}
