//! The API of a running graph, the same on every host.

use std::future::Future;

use futures::future::Either;
use witgraph_ir::{CompiledGraph, NodeId, PortName};

use crate::error::RuntimeError;
use crate::executor::MaybeSend;
use crate::node::NodeState;
use crate::schedule::{FaultReport, TickResult};
use crate::snapshot::{Snapshot, SnapshotError};

/// Which value of an output port is latched. It moves on each time the node's
/// `run` writes the port, or a snapshot is restored, so a host can tell it has
/// seen the latest without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct PortVersion(u64);

impl PortVersion {
    /// The version `value`.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The version as a number: 0 until the port is first written.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// `pull` (any of a graph's reads and writes), unless `stop` completes first,
/// which is [`RuntimeError::PullStopped`]. A pull waits for the guest to suspend
/// when one is spinning, forever with no fuel budget, so give it a timer here if
/// that could matter. Stopping a pull is safe: a read is forgotten, and a write
/// still happens, as if it had been waited for.
pub async fn until<T>(
    stop: impl Future<Output = ()>,
    pull: impl Future<Output = Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    futures::pin_mut!(stop, pull);
    match futures::future::select(pull, stop).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(RuntimeError::PullStopped),
    }
}

/// Reads a graph's latched outputs, when the host chooses: nothing is pushed
/// to the host. A reader works while a tick is running too.
pub trait OutputReader {
    /// What a Value port's value is to the host.
    type Value;
    /// What the host holds of a handle port's object.
    type Handle;

    /// The latched value of a Value output port: what the node's `run`
    /// returned last, or `None` if it has not run yet. An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is no Value output
    /// ([`RuntimeError::NotAValuePort`]) is an error.
    fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<Self::Value>, RuntimeError>> + MaybeSend;

    /// [`read_output`](Self::read_output), as WAVE text.
    fn read_output_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<String>, RuntimeError>> + MaybeSend;

    /// The latched object of a handle output port, or `None` if the node has
    /// not run yet. A port that is no handle output is
    /// [`RuntimeError::NotAValuePort`].
    fn read_handle(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<Self::Handle>, RuntimeError>> + MaybeSend;

    /// The latched value of a Value input port: what its node runs on next, or
    /// `None` while it has none. What a node breakpoint shows of a held run. A
    /// port that is no Value input is [`RuntimeError::NotAValuePort`].
    fn read_input(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<Self::Value>, RuntimeError>> + MaybeSend;

    /// [`read_input`](Self::read_input), as WAVE text.
    fn read_input_wave(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<String>, RuntimeError>> + MaybeSend;

    /// Which value of a Value or handle output port is latched. It asks no
    /// guest anything, so it costs next to nothing. The same errors as
    /// [`read_output`](Self::read_output).
    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError>;
}

/// A compiled graph loaded on a host, ready to run.
pub trait RunningGraph {
    /// What a Value port's value is to the host.
    type Value;
    /// What the host holds of a handle port's object.
    type Handle;
    /// What reads this graph's outputs while it runs.
    type Reader: OutputReader<Value = Self::Value, Handle = Self::Handle> + Clone;

    /// What reads this graph's latched outputs, while a tick runs too.
    fn reader(&self) -> Self::Reader;

    /// Writes a value to a Value input port, as an external source would.
    /// The port must be unconnected or fed only by feedback connections. A
    /// value equal to the current one is not a change; otherwise the node
    /// re-runs on the next tick.
    fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        value: Self::Value,
    ) -> impl Future<Output = Result<(), RuntimeError>> + MaybeSend;

    /// [`inject`](Self::inject)s a value written as WAVE text.
    fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + MaybeSend;

    /// Clears an injected Value input, as if nothing had been injected.
    fn clear_input(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<(), RuntimeError>> + MaybeSend;

    /// The latched value of a Value output port; see
    /// [`OutputReader::read_output`].
    fn read_output(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<Self::Value>, RuntimeError>> + MaybeSend;

    /// [`read_output`](Self::read_output), as WAVE text.
    fn read_output_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<String>, RuntimeError>> + MaybeSend;

    /// The latched object of a handle output port; see
    /// [`OutputReader::read_handle`].
    fn read_handle(
        &mut self,
        node: &NodeId,
        port: &PortName,
    ) -> impl Future<Output = Result<Option<Self::Handle>, RuntimeError>> + MaybeSend;

    /// See [`OutputReader::output_version`].
    fn output_version(&self, node: &NodeId, port: &PortName) -> Result<PortVersion, RuntimeError>;

    /// Makes the node owe a run on its latched inputs, as an input change
    /// would; a stopped graph is rebuilt first.
    fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError>;

    /// A node's current lifecycle state.
    fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError>;

    /// Takes the fault reported since the last call, if there is one: any fault
    /// stops the whole graph.
    fn take_faults(&mut self) -> Vec<FaultReport>;

    /// Takes in what happened since the last tick, starting nothing, so that
    /// [`node_state`](Self::node_state) says what is so between ticks: a `run` that has
    /// returned, or a stream group's background work having ended. A tick that ended
    /// early can leave a graph running; call this to follow it. It may take more than
    /// one call to see the work end.
    fn catch_up(&mut self);

    /// Stops everything: what is in flight is dropped and all owed work is
    /// forgotten. A later input change or [`rerun`](Self::rerun) starts again.
    fn shutdown(&mut self);

    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections. Dropping the future is safe: what is in flight
    /// resumes on the next tick.
    fn tick(&mut self) -> impl Future<Output = TickResult> + MaybeSend;

    /// Like [`tick`](Self::tick), but ends early when `stop` completes, with
    /// [`TickResult::Interrupted`].
    fn tick_until(
        &mut self,
        stop: impl Future<Output = ()> + MaybeSend,
    ) -> impl Future<Output = TickResult> + MaybeSend;

    /// Captures the graph's host-visible state.
    fn snapshot(&mut self) -> impl Future<Output = Result<Snapshot, SnapshotError>> + MaybeSend;

    /// Replaces the graph's host-visible state with `snapshot`.
    fn restore(
        &mut self,
        snapshot: &Snapshot,
    ) -> impl Future<Output = Result<(), RuntimeError>> + MaybeSend;

    /// The compiled graph this was loaded from.
    fn compiled(&self) -> &CompiledGraph;
}
