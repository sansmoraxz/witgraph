//! Bounded FIFO event queue.

use std::collections::VecDeque;

use crate::error::ChannelError;
use witgraph_ir::Val;

/// A bounded FIFO queue for discrete event payloads.
///
/// Events are enqueued by producers and dequeued by consumers. The
/// queue has a configurable maximum capacity; enqueuing at capacity
/// returns [`ChannelError::Full`].
#[derive(Debug, Clone)]
pub struct EventQueue {
    queue: VecDeque<Val>,
    capacity: usize,
}

impl EventQueue {
    /// Creates an empty queue with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Enqueues a value. Returns [`ChannelError::Full`] when the queue
    /// is at capacity.
    pub fn enqueue(&mut self, val: Val) -> Result<(), ChannelError> {
        if self.queue.len() >= self.capacity {
            return Err(ChannelError::Full(self.capacity));
        }
        self.queue.push_back(val);
        Ok(())
    }

    /// Dequeues the oldest value, or `None` if the queue is empty.
    pub fn dequeue(&mut self) -> Option<Val> {
        self.queue.pop_front()
    }

    /// Returns `true` if the queue has no pending events.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The number of events currently in the queue.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// The maximum number of events the queue can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_ordering() {
        let mut q = EventQueue::new(4);
        q.enqueue(Val::U32(1)).unwrap();
        q.enqueue(Val::U32(2)).unwrap();
        q.enqueue(Val::U32(3)).unwrap();
        assert_eq!(q.dequeue(), Some(Val::U32(1)));
        assert_eq!(q.dequeue(), Some(Val::U32(2)));
        assert_eq!(q.dequeue(), Some(Val::U32(3)));
        assert_eq!(q.dequeue(), None);
    }

    #[test]
    fn empty_queue() {
        let q = EventQueue::new(8);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn capacity_limit() {
        let mut q = EventQueue::new(2);
        q.enqueue(Val::Bool(true)).unwrap();
        q.enqueue(Val::Bool(false)).unwrap();
        let err = q.enqueue(Val::Bool(true)).unwrap_err();
        assert!(matches!(err, ChannelError::Full(2)));
    }

    #[test]
    fn dequeue_frees_capacity() {
        let mut q = EventQueue::new(1);
        q.enqueue(Val::U8(1)).unwrap();
        assert!(q.enqueue(Val::U8(2)).is_err());
        q.dequeue();
        q.enqueue(Val::U8(3)).unwrap();
        assert_eq!(q.dequeue(), Some(Val::U8(3)));
    }
}
