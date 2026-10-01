//! Activation interface: the contract between the scheduler and node
//! instances.
//!
//! [`Activation`] describes why the scheduler is activating a node.
//! [`InputSnapshot`] provides a frozen read-only view of the node's
//! latched value inputs. [`OutputCollector`] is a transactional buffer
//! that accumulates output writes during an activation and is committed
//! or discarded atomically.

use std::collections::HashMap;

use witgraph_ir::PortName;

use witgraph_ir::Val;

/// The reason a node is being activated.
#[derive(Debug, Clone)]
pub enum Activation {
    /// Synchronous activation: all inputs are latched values.
    Sync,
    /// A drained input received an item during the drain phase.
    DrainItem {
        /// The port receiving the drained item.
        port: PortName,
        /// The drained item.
        item: Val,
    },
    /// A stream item arrived on a port.
    StreamItem {
        /// The port receiving the stream item.
        port: PortName,
        /// The stream item.
        item: Val,
    },
    /// An event arrived on a port.
    Event {
        /// The port receiving the event.
        port: PortName,
        /// The event payload.
        payload: Val,
    },
    /// A future resolved on a port.
    FutureResolved {
        /// The port whose future resolved.
        port: PortName,
        /// The resolved value.
        value: Val,
    },
    /// A stream closed on a port (end-of-stream).
    StreamClosed {
        /// The port whose stream closed.
        port: PortName,
    },
}

/// What the node wants after an activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationResult {
    /// The node wants more activations.
    Continue,
    /// The node is done; outputs are finalized.
    Completed,
}

/// Read-only view of a node's current latched value inputs.
///
/// Built by the scheduler from the current channel state immediately
/// before each activation. Host `read-value` calls read from the
/// snapshot, not live channels.
#[derive(Debug, Clone, Default)]
pub struct InputSnapshot {
    values: HashMap<PortName, Val>,
}

impl InputSnapshot {
    /// Creates an empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a latched value for the given port.
    pub fn insert(&mut self, port: PortName, val: Val) {
        self.values.insert(port, val);
    }

    /// Reads the latched value for a port, returning `None` for
    /// unconnected optional inputs or inputs not yet written.
    pub fn read_value(&self, port: &PortName) -> Option<&Val> {
        self.values.get(port)
    }

    /// Returns `true` if the snapshot already has a value for the port.
    pub fn contains(&self, port: &PortName) -> bool {
        self.values.contains_key(port)
    }
}

/// A single output write buffered in an [`OutputCollector`].
#[derive(Debug, Clone)]
pub enum OutputWrite {
    /// Write a latched value output.
    Value {
        /// The output port.
        port: PortName,
        /// The value to latch.
        value: Val,
    },
    /// Emit a discrete event.
    Event {
        /// The output port.
        port: PortName,
        /// The event payload.
        payload: Val,
    },
    /// Push an item to a stream output.
    StreamPush {
        /// The output port.
        port: PortName,
        /// The stream item.
        item: Val,
    },
    /// Close a stream output (end-of-stream).
    StreamClose {
        /// The output port.
        port: PortName,
    },
    /// Resolve a future output.
    FutureResolve {
        /// The output port.
        port: PortName,
        /// The resolved value.
        value: Val,
    },
}

/// Transactional buffer for a node's output writes during an activation.
///
/// Host functions write to the collector, not directly to channels.
/// After a successful activation the scheduler commits the collected
/// writes to channels. On fault (trap or error) the collector is
/// discarded, preventing partial outputs from leaking to downstream
/// nodes.
#[derive(Debug, Clone, Default)]
pub struct OutputCollector {
    writes: Vec<OutputWrite>,
    fatal: Option<String>,
}

impl OutputCollector {
    /// Creates an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffers an output write.
    pub fn write(&mut self, output: OutputWrite) {
        self.writes.push(output);
    }

    /// Signals a graph-fatal condition. The tick halts immediately.
    pub fn set_fatal(&mut self, message: String) {
        self.fatal = Some(message);
    }

    /// Returns the fatal message, if one was signaled.
    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// Consumes the collector and returns all buffered writes.
    pub fn drain(self) -> Vec<OutputWrite> {
        self.writes
    }

    /// Returns `true` if no writes have been buffered.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// The number of buffered writes.
    pub fn len(&self) -> usize {
        self.writes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_buffers_writes() {
        let mut collector = OutputCollector::new();
        assert!(collector.is_empty());

        collector.write(OutputWrite::Value {
            port: "out".into(),
            value: Val::U32(42),
        });
        collector.write(OutputWrite::Event {
            port: "sig".into(),
            payload: Val::Bool(true),
        });

        assert_eq!(collector.len(), 2);
        assert!(!collector.is_empty());
    }

    #[test]
    fn collector_drain_consumes() {
        let mut collector = OutputCollector::new();
        collector.write(OutputWrite::Value {
            port: "out".into(),
            value: Val::U32(1),
        });
        collector.write(OutputWrite::StreamPush {
            port: "data".into(),
            item: Val::U8(0),
        });

        let writes = collector.drain();
        assert_eq!(writes.len(), 2);
    }

    #[test]
    fn collector_fatal_signals_halt() {
        let mut collector = OutputCollector::new();
        assert!(collector.fatal().is_none());

        collector.set_fatal("invariant violated".into());
        assert_eq!(collector.fatal(), Some("invariant violated"));
    }

    #[test]
    fn discard_on_fault() {
        let mut collector = OutputCollector::new();
        collector.write(OutputWrite::Value {
            port: "out".into(),
            value: Val::U32(1),
        });
        // On fault, the collector is simply dropped; no writes leak.
        drop(collector);
    }

    #[test]
    fn snapshot_read_write() {
        let mut snapshot = InputSnapshot::new();
        assert!(snapshot.read_value(&"in".into()).is_none());

        snapshot.insert("in".into(), Val::F64(2.5));
        assert_eq!(snapshot.read_value(&"in".into()), Some(&Val::F64(2.5)));
    }

    #[test]
    fn snapshot_unconnected_returns_none() {
        let snapshot = InputSnapshot::new();
        assert!(snapshot.read_value(&"missing".into()).is_none());
    }
}
