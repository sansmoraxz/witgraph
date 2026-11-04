//! A typed layer over [`SchedulerCore`] that holds a graph's values in the host
//! ([`crate::latches`]), for an [`Executor`] that only calls the nodes. The
//! scheduling model is described on [`witgraph_sched::control`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use witgraph_ir::{CompiledGraph, InputIdx, NodeId, PortDirection, PortName};

use witgraph_traits::PortVersion;

use witgraph_sched::SchedulerCore;
use witgraph_traits::{DataPlane, FaultReport, NodeState, RuntimeError, RuntimeMode, TickResult};

use crate::executor::{Canonical, Executor};
use crate::latches::{HostPlane, LatchReader, plan_members};
use crate::plan::RunShape;

/// A compiled graph being run, with its values held in the host: the last stage
/// of the typestate chain `GraphBuilder -> Graph -> CompiledGraph`, over an
/// [`Executor`].
///
/// `M` is the instrumentation mode; `X` runs the nodes.
pub struct Scheduler<M: RuntimeMode, X: Executor> {
    mode: Arc<M>,
    pub(crate) core: SchedulerCore<HostPlane<X>>,
}

impl<M: RuntimeMode, X: Executor> Scheduler<M, X> {
    /// The scheduling logic, over its data plane as it sees it.
    pub(crate) fn core(&mut self) -> &mut SchedulerCore<dyn DataPlane> {
        &mut self.core
    }

    /// A scheduler over a compiled graph whose nodes `executor` has built.
    ///
    /// `shapes` holds the shape of each node's `run`. Every node owes its first
    /// run. `max_steps_per_tick` is at least 1
    /// ([`RuntimeError::InvalidConfig`]).
    pub fn new(
        compiled: CompiledGraph,
        mode: M,
        executor: X,
        shapes: &HashMap<NodeId, RunShape>,
        max_steps_per_tick: usize,
    ) -> Result<Self, RuntimeError> {
        let mode = Arc::new(mode);
        let members = plan_members(&compiled, &witgraph_ir::PortTable::new(&compiled), shapes)?;
        let core = SchedulerCore::new(
            compiled,
            mode.clone(),
            max_steps_per_tick,
            |compiled, ports| HostPlane::new(executor, compiled, ports, members),
        )?;
        Ok(Self { mode, core })
    }

    /// Writes a value to a Value input port, as an external source would.
    ///
    /// The port must be unconnected or fed only by feedback connections:
    /// an input has one writer, so one a connection writes is rejected
    /// ([`RuntimeError::ConnectedInput`]). The value must have the port's
    /// payload type (for an optional port, the inner type). Writing a value
    /// equal to the current one is not a change and does nothing; otherwise
    /// the node re-runs on the next tick (a stopped graph is rebuilt first),
    /// and the write wins over any feedback for this input computed before it.
    pub fn inject(
        &mut self,
        node: &NodeId,
        port: &PortName,
        val: X::Value,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.host_input(node, port)?;
        let val = self
            .core
            .plane
            .executor
            .check_input(ty, val)
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })?;
        self.inject_checked(input, val);
        Ok(())
    }

    /// Writes a value already in the port's checked, canonical form.
    fn inject_checked(&mut self, input: InputIdx, val: Canonical<X::Value>) {
        self.core().serve_queued();
        let changed = self
            .core
            .plane
            .lock()
            .host_set(input, Arc::new(val.into_inner()));
        if changed {
            self.core().host_set(input);
        } else {
            self.core().host_kept(input);
        }
    }

    /// [`inject`](Self::inject)s a value written as WAVE text, parsed
    /// against the port's payload type ([`RuntimeError::ValueType`] if it
    /// does not parse). Record fields, cases and flags the type does not
    /// have are rejected.
    pub fn inject_wave(
        &mut self,
        node: &NodeId,
        port: &PortName,
        text: &str,
    ) -> Result<(), RuntimeError> {
        let (input, ty) = self.host_input(node, port)?;
        let val = self
            .core
            .plane
            .executor
            .parse_wave(ty, text)
            .map_err(|message| RuntimeError::ValueType {
                node: node.clone(),
                port: port.clone(),
                message,
            })?;
        self.inject_checked(input, val);
        Ok(())
    }

    /// Clears an injected Value input, as if nothing had been injected: an
    /// optional input reads as `none` again, and a required one leaves its
    /// node waiting for a value. The same ports as [`inject`](Self::inject)
    /// qualify. Clearing an input that has no value is not a change and does
    /// nothing; otherwise the node re-runs, and the clear wins over feedback
    /// for this input computed before it.
    pub fn clear_input(&mut self, node: &NodeId, port: &PortName) -> Result<(), RuntimeError> {
        self.core().serve_queued();
        let (input, _) = self.host_input(node, port)?;
        let cleared = self.core.plane.lock().host_clear(input);
        if cleared {
            self.core().host_cleared(input);
        }
        Ok(())
    }

    /// Makes the node owe a run on its latched inputs, as an input change
    /// would; a stopped graph is rebuilt first. This is how a node whose inputs
    /// the host cannot change (a source driven by a capability, say) runs again
    /// after a fault or shutdown.
    pub fn rerun(&mut self, node: &NodeId) -> Result<(), RuntimeError> {
        self.core().rerun(node)
    }

    /// What reads this graph's latched outputs at any time, while a tick
    /// holds the scheduler too.
    pub fn reader(&self) -> LatchReader<X::Value> {
        LatchReader {
            latches: self.core.plane.latches.clone(),
            ports: self.core.ports().clone(),
            versions: self.core.versions().clone(),
        }
    }

    /// The latched value of a Value or handle output port: what the node's
    /// `run` returned last, or `None` if it has not run yet. An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is neither
    /// ([`RuntimeError::NotAValuePort`]) is an error.
    pub fn read_output(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<Option<X::Value>, RuntimeError> {
        self.reader().read_output(node, port)
    }

    /// Which value of a Value or handle output port is latched: it moves on
    /// each time the node's `run` writes the port (or a snapshot is
    /// restored), so a host can tell it has seen the latest without reading
    /// it. The same errors as [`read_output`](Self::read_output).
    pub fn output_version(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<PortVersion, RuntimeError> {
        self.reader().output_version(node, port)
    }

    /// The type of a Value port: for an input, its payload type (for an
    /// optional port, the inner type). An unknown node
    /// ([`RuntimeError::UnknownNode`]) or a port that is not a Value port of
    /// that direction ([`RuntimeError::NotAValuePort`]) is an error.
    pub fn port_type(
        &self,
        node: &NodeId,
        port: &PortName,
        direction: PortDirection,
    ) -> Result<&X::Type, RuntimeError> {
        self.value_port(node, port, direction)
    }

    /// The payload type of a Value input the host may write, as
    /// [`inject`](Self::inject) checks it: an error for an input a
    /// connection writes ([`RuntimeError::ConnectedInput`]) as for one that
    /// is no Value input. For an executor that converts a value before
    /// injecting it.
    pub fn host_input_type(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<&X::Type, RuntimeError> {
        self.host_input(node, port).map(|(_, ty)| ty)
    }

    /// Checks that `node` exists and `port` is a Value port of it in
    /// `direction`, and finds its type.
    fn value_port(
        &self,
        node: &NodeId,
        port: &PortName,
        direction: PortDirection,
    ) -> Result<&X::Type, RuntimeError> {
        let ports = self.core.ports();
        if ports.node(node).is_none() {
            return Err(RuntimeError::UnknownNode { node: node.clone() });
        }
        let entry = match direction {
            PortDirection::Input => ports.input(node, port).map(|input| &ports.inputs()[input]),
            PortDirection::Output => ports
                .output(node, port)
                .map(|output| &ports.outputs()[output]),
        };
        entry
            .and_then(|entry| self.core.plane.executor.port_type(&entry.port, direction))
            .ok_or_else(|| RuntimeError::NotAValuePort {
                node: node.clone(),
                port: port.clone(),
                direction,
            })
    }

    /// A Value input the host may write ([`inject`](Self::inject),
    /// [`clear_input`](Self::clear_input)).
    fn host_input(
        &self,
        node: &NodeId,
        port: &PortName,
    ) -> Result<(InputIdx, &X::Type), RuntimeError> {
        let ty = self.value_port(node, port, PortDirection::Input)?;
        let not_a_port = || RuntimeError::NotAValuePort {
            node: node.clone(),
            port: port.clone(),
            direction: PortDirection::Input,
        };
        let input = self.core.ports().input(node, port).ok_or_else(not_a_port)?;
        if let Some(connection) = self.core.connected(input) {
            return Err(RuntimeError::ConnectedInput {
                node: node.clone(),
                port: port.clone(),
                connection: connection.clone(),
            });
        }
        Ok((input, ty))
    }

    /// A node's current lifecycle state (see [`crate::node`]). An unknown node
    /// is an error ([`RuntimeError::UnknownNode`]).
    pub fn node_state(&self, node: &NodeId) -> Result<NodeState, RuntimeError> {
        let core: &SchedulerCore<dyn DataPlane> = &self.core;
        core.node_state(node)
    }

    /// Takes the fault reported since the last call, if there is one: a tick that
    /// saw it returned it too ([`TickResult::Faulted`]). It is also in
    /// [`node_state`](Self::node_state) while the graph stays stopped.
    pub fn take_faults(&mut self) -> Vec<FaultReport> {
        self.core().take_faults()
    }

    /// Stops everything: drops the instance and all it held, and forgets all
    /// owed work. A faulted graph keeps its fault. A later input change or
    /// [`rerun`](Self::rerun) builds a new instance.
    pub fn shutdown(&mut self) {
        self.core().shutdown();
    }

    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections.
    ///
    /// Starts every node that can start, delivers Value outputs as `run`s
    /// return, and keeps starting nodes whose inputs changed, until nothing is
    /// in flight and nothing can start. Then each feedback connection's latest
    /// value is written to its target (one loop iteration). A tick that returns
    /// [`TickResult::StepLimitReached`] or [`TickResult::Faulted`] latches
    /// nothing; a later tick finishes the iteration.
    ///
    /// Work that never finishes (an endless stream, say) keeps the tick running:
    /// drive such a graph with [`tick_until`](Self::tick_until). Dropping the
    /// tick's future is safe and ends it as `tick_until` ends an interrupted
    /// one: what is in flight persists and resumes on the next tick.
    pub async fn tick(&mut self) -> TickResult {
        self.core().tick().await
    }

    /// Like [`tick`](Self::tick), but ends early when `stop` completes (a
    /// timer, say), with [`TickResult::Interrupted`]. What is in flight carries
    /// on with the next tick. Every buffered feedback value whose iteration is
    /// over is latched as the tick ends: the value's target has settled (it is
    /// not running, cannot start, and nothing upstream of it can), and so has
    /// every feedback source into it. So a feedback loop advances one iteration
    /// per tick even beside work that never finishes.
    pub async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        self.core().tick_until(stop).await
    }

    /// The compiled graph being run.
    pub fn compiled(&self) -> &CompiledGraph {
        self.core.compiled()
    }

    /// The runtime mode (for [`Trace`](crate::Trace), its trace).
    pub fn mode(&self) -> &M {
        &self.mode
    }

    /// What runs the nodes.
    pub fn executor(&self) -> &X {
        &self.core.plane.executor
    }

    /// The most runs one tick may start before it returns
    /// [`TickResult::StepLimitReached`].
    pub fn max_steps_per_tick(&self) -> usize {
        self.core.max_steps_per_tick()
    }

    /// Sets [`max_steps_per_tick`](Self::max_steps_per_tick); at least 1
    /// ([`RuntimeError::InvalidConfig`]).
    pub fn set_max_steps_per_tick(&mut self, steps: usize) -> Result<(), RuntimeError> {
        self.core.set_max_steps_per_tick(steps)
    }
}
