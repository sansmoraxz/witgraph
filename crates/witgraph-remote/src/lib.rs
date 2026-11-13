//! Lets a debugger attach to a graph running on wasmtime, over wRPC on WebTransport.
//!
//! A program running a graph wraps it in a [`Debuggee`] and ticks it through that, then
//! [`serve`]s it. A debugger, such as the editor, attaches with the URL, certificate hash
//! and token [`Served`] says, and can pause, step, hold nodes, stop on lines, read outputs
//! and write inputs. It cannot change the graph.
//!
//! Anything it asks that needs the graph while a tick runs ends that tick early, is done, and
//! the tick goes on: a held node keeps a tick from ending, and the debugger must see in then.
//! Dropping or interrupting a tick loses nothing ([`RunningGraph::tick_until`]).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::select_all;
use tokio::sync::{Notify, oneshot};
use witgraph_ir::{NodeId, PortName};
use witgraph_runtime::witgraph_traits::{NodeFault, NodePhase, RuntimeMode, Trace, TraceEvent};
use witgraph_runtime::{
    HoldAt, HoldId, Host, LineDebugger, NodeHolds, OutputReader, RunningGraph, RuntimeGraph,
    RuntimeHolds, RuntimeLines, StopReason, TickResult, ValReader,
};

mod bindings {
    #![allow(missing_docs, clippy::all)]
    wit_bindgen_wrpc::generate!({
        world: "debuggee",
        path: "wit",
        generate_all,
    });
}

use bindings::exports::witgraph::debug::{graph, holds, lines};

/// The most trace events kept for a debugger to take: as many as the editor's timeline shows.
const TRACE_KEPT: usize = 1000;

/// The mode a graph a debugger can attach to runs in: the program's own, and a trace of the
/// latest events for the debugger's timeline. Load the graph with it, as
/// `RuntimeGraph::load(compiled, &wasm, config, Watched::new(Perf))`.
pub struct Watched<M = witgraph_runtime::Perf> {
    mode: M,
    trace: Arc<Trace>,
}

impl<M: RuntimeMode> Watched<M> {
    /// The program's own mode, watched.
    pub fn new(mode: M) -> Self {
        Self {
            mode,
            trace: Arc::new(Trace::bounded(TRACE_KEPT)),
        }
    }

    /// The program's own mode.
    pub fn mode(&self) -> &M {
        &self.mode
    }
}

impl<M: RuntimeMode> RuntimeMode for Watched<M> {
    fn on_run_started(&self, node: &NodeId) {
        self.mode.on_run_started(node);
        self.trace.on_run_started(node);
    }

    fn on_run_returned(&self, node: &NodeId) {
        self.mode.on_run_returned(node);
        self.trace.on_run_returned(node);
    }

    fn on_group_drained(&self, nodes: &[NodeId]) {
        self.mode.on_group_drained(nodes);
        self.trace.on_group_drained(nodes);
    }

    fn on_phase_transition(&self, node: &NodeId, from: NodePhase, to: NodePhase) {
        self.mode.on_phase_transition(node, from, to);
        self.trace.on_phase_transition(node, from, to);
    }

    fn on_node_fault(&self, node: &NodeId, fault: &NodeFault) {
        self.mode.on_node_fault(node, fault);
        self.trace.on_node_fault(node, fault);
    }

    fn on_cancelled(&self, node: &NodeId) {
        self.mode.on_cancelled(node);
        self.trace.on_cancelled(node);
    }

    fn on_restarted(&self, node: &NodeId) {
        self.mode.on_restarted(node);
        self.trace.on_restarted(node);
    }
}

/// Something asked of the graph, done when nothing else has it.
type Job<M, H> =
    Box<dyn for<'a> FnOnce(&'a mut RuntimeGraph<Watched<M>, H>) -> BoxFuture<'a, ()> + Send>;

struct Inner<M: RuntimeMode, H: Host> {
    graph: tokio::sync::Mutex<RuntimeGraph<Watched<M>, H>>,
    /// What waits for the graph, in the order asked.
    jobs: Mutex<Vec<Job<M, H>>>,
    /// Ends a tick early, for what waits.
    asked: Notify,
    /// Whether a debugger has stopped the graph: its ticks then advance nothing.
    stopped: Arc<AtomicBool>,
    reader: ValReader,
    trace: Arc<Trace>,
    holds: RuntimeHolds,
    lines: Option<RuntimeLines>,
    described: String,
    contracts: String,
    groups: Vec<Vec<String>>,
}

/// The version of the debug protocol, `wit/debug.wit`, that this crate speaks. A debugger that
/// speaks another refuses to attach, as calls the other side does not know would fail. It moves on
/// with each change to the protocol, which a test makes sure of, and with each change to what its
/// JSON says (the contracts' shape, say), which none can.
pub const PROTOCOL: u32 = 4;

/// A graph a debugger can attach to. Tick it through this, not the graph itself.
pub struct Debuggee<M: RuntimeMode, H: Host> {
    inner: Arc<Inner<M, H>>,
}

impl<M: RuntimeMode, H: Host> Clone for Debuggee<M, H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: RuntimeMode, H: Host> Debuggee<M, H> {
    /// Wraps a loaded graph.
    ///
    /// # Errors
    ///
    /// If the graph cannot be described as JSON, which should not happen.
    pub fn new(graph: RuntimeGraph<Watched<M>, H>) -> anyhow::Result<Self> {
        let described = serde_json::to_string(graph.compiled().graph())
            .context("failed to describe the graph")?;
        let compiled = graph.compiled();
        let contracts: serde_json::Map<String, serde_json::Value> = compiled
            .graph()
            .nodes
            .iter()
            .filter_map(|node| {
                let contract = compiled.contract_for(&node.id)?;
                let meta = witgraph_wit::metadata::ComponentMeta::of(contract);
                Some(serde_json::to_value(meta).map(|meta| (node.id.to_string(), meta)))
            })
            .collect::<Result<_, _>>()
            .context("failed to describe the contracts")?;
        let contracts =
            serde_json::to_string(&contracts).context("failed to describe the contracts")?;
        let groups = graph
            .compiled()
            .stream_groups()
            .iter()
            .map(|group| group.iter().map(ToString::to_string).collect())
            .collect();
        Ok(Self {
            inner: Arc::new(Inner {
                reader: graph.reader(),
                trace: graph.mode().trace.clone(),
                holds: graph.holds(),
                lines: graph.lines(),
                graph: tokio::sync::Mutex::new(graph),
                jobs: Mutex::new(Vec::new()),
                asked: Notify::new(),
                stopped: Arc::new(AtomicBool::new(false)),
                described,
                contracts,
                groups,
            }),
        })
    }

    /// Runs one tick, as [`RunningGraph::tick`] does, doing what a debugger asks meanwhile.
    /// With the graph stopped by a debugger, it does only what is asked, and says `Idle`: what
    /// was under way waits where it is until the debugger runs the graph again.
    pub async fn tick(&self) -> TickResult {
        let mut graph = self.inner.graph.lock().await;
        loop {
            if self.stopped() {
                self.drain(&mut graph).await;
                return TickResult::Idle;
            }
            self.drain(&mut graph).await;
            let asked = self.inner.asked.notified();
            let result = graph.tick_until(asked).await;
            let waiting = !lock(&self.inner.jobs).is_empty();
            if matches!(result, TickResult::Interrupted) && waiting {
                continue;
            }
            self.drain(&mut graph).await;
            return result;
        }
    }

    /// Whether a debugger has stopped the graph, so that its ticks advance nothing.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// Stops the graph: its ticks advance nothing until a debugger runs it. Nothing is under
    /// way in a graph just loaded, which a program loading one in place of a graph a debugger
    /// stopped keeps stopped so; a debugger's `stop` also cancels what is
    /// ([`stop_and_cancel`](Self::stop_and_cancel)).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        // The tick under way ends now.
        self.inner.asked.notify_one();
    }

    /// Stops the graph as a debugger's `stop` does, as a graph run in the editor's page stops:
    /// its ticks advance nothing, and what is under way is cancelled (its nodes say so), to run
    /// again on their latched inputs once the graph runs ([`run_again`](Self::run_again)).
    pub async fn stop_and_cancel(&self) {
        self.stop();
        // What a debugger holds, a run before it starts or a guest on a line, is let go, as the
        // tick under way waits on it: then it ends, and what it ran is cancelled. Breakpoints
        // go too; a debugger sets its own again when it runs the graph.
        for run in self.inner.holds.held() {
            self.inner.holds.resume(run.id);
        }
        if let Some(lines) = &self.inner.lines {
            lines.release();
        }
        self.with(|graph| {
            Box::pin(async move {
                graph.shutdown();
            })
        })
        .await;
    }

    /// Runs the graph again, as a debugger's `run` does: the nodes a stop cancelled run again
    /// on their latched inputs, and its ticks advance it.
    pub async fn run_again(&self) {
        self.with(|graph| {
            Box::pin(async move {
                let cancelled: Vec<NodeId> = graph
                    .compiled()
                    .graph()
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .filter(|node| {
                        graph
                            .node_state(node)
                            .is_ok_and(|state| state.phase() == NodePhase::Cancelled)
                    })
                    .collect();
                for node in cancelled {
                    let _ = graph.rerun(&node);
                }
            })
        })
        .await;
        self.inner.stopped.store(false, Ordering::SeqCst);
    }

    /// Lets go of what a debugger that went held: runs held, guests stopped on lines and
    /// breakpoints, and runs the graph again if it stopped it, what the stop cancelled too: the
    /// program is not left waiting on a debugger that is gone. [`serve`] does it as each goes;
    /// a program serving the calls itself ([`handle`]) does it.
    pub fn debugger_gone(&self) {
        self.inner.holds.release();
        if let Some(lines) = &self.inner.lines {
            lines.release();
        }
        let debuggee = self.clone();
        tokio::spawn(async move { debuggee.run_again().await });
    }

    /// Does `work` on the graph, between ticks or by ending one early: as the program running
    /// it writes its inputs, say.
    pub async fn with<T: Send + 'static>(
        &self,
        work: impl for<'a> FnOnce(&'a mut RuntimeGraph<Watched<M>, H>) -> BoxFuture<'a, T>
        + Send
        + 'static,
    ) -> T {
        let (done, result) = oneshot::channel();
        lock(&self.inner.jobs).push(Box::new(move |graph| {
            Box::pin(async move {
                let _ = done.send(work(graph).await);
            })
        }));
        self.inner.asked.notify_one();
        // Done here when no tick has the graph; otherwise the tick does it.
        if let Ok(mut graph) = self.inner.graph.try_lock() {
            self.drain(&mut graph).await;
        }
        match result.await {
            Ok(value) => value,
            // The graph went away with what was asked: nothing more can come of it.
            Err(_) => std::future::pending().await,
        }
    }

    async fn drain(&self, graph: &mut RuntimeGraph<Watched<M>, H>) {
        loop {
            let jobs = std::mem::take(&mut *lock(&self.inner.jobs));
            if jobs.is_empty() {
                return;
            }
            for job in jobs {
                job(graph).await;
            }
        }
    }
}

/// A node's phase, as both hosts name it.
fn phase_name(phase: NodePhase) -> String {
    serde_json::to_value(phase)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn trace_event(event: TraceEvent) -> graph::TraceEvent {
    use graph::TraceEvent as Event;
    match event {
        TraceEvent::RunStarted { node } => Event::RunStarted(node.to_string()),
        TraceEvent::RunReturned { node } => Event::RunReturned(node.to_string()),
        TraceEvent::GroupDrained { nodes } => {
            Event::GroupDrained(nodes.iter().map(ToString::to_string).collect())
        }
        TraceEvent::PhaseTransition { node, from, to } => {
            Event::PhaseTransition(graph::Transition {
                node: node.to_string(),
                before: phase_name(from),
                after: phase_name(to),
            })
        }
        TraceEvent::Fault { node, fault } => Event::Fault(graph::NodeFault {
            node: node.to_string(),
            kind: fault.kind().to_string(),
            message: fault.to_string(),
        }),
        TraceEvent::Cancelled { node } => Event::Cancelled(node.to_string()),
        TraceEvent::Restarted { node } => Event::Restarted(node.to_string()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serves a [`Debuggee`]'s calls.
struct Handler<M: RuntimeMode, H: Host> {
    debuggee: Debuggee<M, H>,
}

impl<M: RuntimeMode, H: Host> Clone for Handler<M, H> {
    fn clone(&self) -> Self {
        Self {
            debuggee: self.debuggee.clone(),
        }
    }
}

impl<M: RuntimeMode, H: Host> Handler<M, H> {
    fn inner(&self) -> &Inner<M, H> {
        &self.debuggee.inner
    }

    fn lines(&self) -> Option<&RuntimeLines> {
        self.inner().lines.as_ref()
    }
}

impl<C: Send + Sync, M: RuntimeMode, H: Host> graph::Handler<C> for Handler<M, H> {
    async fn describe(&self, _: C) -> anyhow::Result<String> {
        Ok(self.inner().described.clone())
    }

    async fn contracts(&self, _: C) -> anyhow::Result<String> {
        Ok(self.inner().contracts.clone())
    }

    async fn stream_groups(&self, _: C) -> anyhow::Result<Vec<Vec<String>>> {
        Ok(self.inner().groups.clone())
    }

    async fn node_state(&self, _: C, node: String) -> anyhow::Result<Result<graph::State, String>> {
        let node = NodeId::from(node);
        Ok(self
            .debuggee
            .with(move |graph| {
                Box::pin(async move {
                    let state = graph.node_state(&node).map_err(|e| e.to_string())?;
                    Ok(graph::State {
                        phase: phase_name(state.phase()),
                        fault: state.fault_cause().map(ToString::to_string),
                        culprit: state.culprit().map(ToString::to_string),
                    })
                })
            })
            .await)
    }

    async fn output_version(
        &self,
        _: C,
        node: String,
        port: String,
    ) -> anyhow::Result<Result<u64, String>> {
        let version = self
            .inner()
            .reader
            .output_version(&NodeId::from(node), &PortName::from(port));
        Ok(version
            .map(|version| version.get())
            .map_err(|e| e.to_string()))
    }

    async fn read_output(
        &self,
        _: C,
        node: String,
        port: String,
    ) -> anyhow::Result<Result<Option<String>, String>> {
        let read = self
            .inner()
            .reader
            .read_output_wave(&NodeId::from(node), &PortName::from(port))
            .await;
        Ok(read.map_err(|e| e.to_string()))
    }

    async fn read_input(
        &self,
        _: C,
        node: String,
        port: String,
    ) -> anyhow::Result<Result<Option<String>, String>> {
        let read = self
            .inner()
            .reader
            .read_input_wave(&NodeId::from(node), &PortName::from(port))
            .await;
        Ok(read.map_err(|e| e.to_string()))
    }

    async fn take_trace(&self, _: C) -> anyhow::Result<Vec<graph::TraceEvent>> {
        Ok(self
            .inner()
            .trace
            .take_trace()
            .into_iter()
            .map(trace_event)
            .collect())
    }

    async fn inject(
        &self,
        _: C,
        node: String,
        port: String,
        wave: String,
    ) -> anyhow::Result<Result<(), String>> {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        Ok(self
            .debuggee
            .with(move |graph| {
                Box::pin(async move {
                    graph
                        .inject_wave(&node, &port, &wave)
                        .await
                        .map_err(|e| e.to_string())
                })
            })
            .await)
    }

    async fn clear_input(
        &self,
        _: C,
        node: String,
        port: String,
    ) -> anyhow::Result<Result<(), String>> {
        let (node, port) = (NodeId::from(node), PortName::from(port));
        Ok(self
            .debuggee
            .with(move |graph| {
                Box::pin(async move {
                    graph
                        .clear_input(&node, &port)
                        .await
                        .map_err(|e| e.to_string())
                })
            })
            .await)
    }

    async fn protocol(&self, _: C) -> anyhow::Result<u32> {
        Ok(PROTOCOL)
    }

    async fn running(&self, _: C) -> anyhow::Result<bool> {
        Ok(!self.debuggee.stopped())
    }

    async fn run(&self, _: C) -> anyhow::Result<()> {
        self.debuggee.run_again().await;
        Ok(())
    }

    async fn stop(&self, _: C) -> anyhow::Result<()> {
        self.debuggee.stop_and_cancel().await;
        Ok(())
    }

    async fn rerun(&self, _: C, node: String) -> anyhow::Result<Result<(), String>> {
        let node = NodeId::from(node);
        Ok(self
            .debuggee
            .with(move |graph| {
                Box::pin(async move { graph.rerun(&node).map_err(|e| e.to_string()) })
            })
            .await)
    }
}

impl<C: Send + Sync, M: RuntimeMode, H: Host> holds::Handler<C> for Handler<M, H> {
    async fn hold_at(&self, _: C, at: holds::Holding) -> anyhow::Result<()> {
        self.inner().holds.hold_at(match at {
            holds::Holding::Nodes(nodes) => {
                HoldAt::Nodes(nodes.into_iter().map(NodeId::from).collect())
            }
            holds::Holding::All => HoldAt::All,
        });
        Ok(())
    }

    async fn held(&self, _: C) -> anyhow::Result<Vec<holds::HeldRun>> {
        Ok(self
            .inner()
            .holds
            .held()
            .into_iter()
            .map(held_run)
            .collect())
    }

    async fn next_held(&self, _: C) -> anyhow::Result<holds::HeldRun> {
        Ok(held_run(self.inner().holds.next_held().await))
    }

    async fn resume(&self, _: C, id: u64) -> anyhow::Result<()> {
        self.inner().holds.resume(HoldId(id));
        Ok(())
    }

    async fn release(&self, _: C) -> anyhow::Result<()> {
        self.inner().holds.release();
        Ok(())
    }
}

fn breakpoint_scope(scope: lines::BreakpointScope) -> witgraph_runtime::BreakpointScope {
    match scope {
        lines::BreakpointScope::Node(node) => {
            witgraph_runtime::BreakpointScope::Node(NodeId::from(node.as_str()))
        }
        lines::BreakpointScope::EveryNode => witgraph_runtime::BreakpointScope::EveryNode,
    }
}

fn held_run(held: witgraph_runtime::HeldRun) -> holds::HeldRun {
    holds::HeldRun {
        id: held.id.0,
        node: held.node.to_string(),
    }
}

impl<C: Send + Sync, M: RuntimeMode, H: Host> lines::Handler<C> for Handler<M, H> {
    async fn files(&self, _: C) -> anyhow::Result<Vec<String>> {
        Ok(self.lines().map(LineDebugger::files).unwrap_or_default())
    }

    async fn files_of(&self, _: C, node: String) -> anyhow::Result<Vec<String>> {
        Ok(self
            .lines()
            .map(|lines| lines.files_of(&NodeId::from(node.as_str())))
            .unwrap_or_default())
    }

    async fn lines(&self, _: C, file: String) -> anyhow::Result<Vec<u32>> {
        Ok(self
            .lines()
            .map(|lines| lines.lines(&file))
            .unwrap_or_default())
    }

    async fn source(&self, _: C, file: String) -> anyhow::Result<Option<String>> {
        // Only what the guests' debug info names: nothing else of the host's files.
        let known = self
            .lines()
            .is_some_and(|lines| lines.files().contains(&file));
        if !known {
            return Ok(None);
        }
        Ok(tokio::fs::read_to_string(&file).await.ok())
    }

    async fn stopped(&self, _: C) -> anyhow::Result<Option<lines::LineStop>> {
        Ok(self.lines().and_then(LineDebugger::stopped).map(line_stop))
    }

    async fn next_stop(&self, _: C) -> anyhow::Result<lines::LineStop> {
        match self.lines() {
            Some(lines) => Ok(line_stop(lines.next_stop().await)),
            // Loaded without debugging lines: no stop ever comes.
            None => std::future::pending().await,
        }
    }

    async fn set_breakpoint(
        &self,
        _: C,
        scope: lines::BreakpointScope,
        file: String,
        line: u32,
    ) -> anyhow::Result<Option<u32>> {
        let scope = breakpoint_scope(scope);
        Ok(self
            .lines()
            .and_then(|lines| lines.set_breakpoint(&scope, &file, line)))
    }

    async fn clear_breakpoint(
        &self,
        _: C,
        scope: lines::BreakpointScope,
        file: String,
        line: u32,
    ) -> anyhow::Result<()> {
        if let Some(lines) = self.lines() {
            lines.clear_breakpoint(&breakpoint_scope(scope), &file, line);
        }
        Ok(())
    }

    async fn pause(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::pause);
        Ok(())
    }

    async fn cancel_pause(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::cancel_pause);
        Ok(())
    }

    async fn resume(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::resume);
        Ok(())
    }

    async fn step_in(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::step_in);
        Ok(())
    }

    async fn step_over(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::step_over);
        Ok(())
    }

    async fn step_out(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::step_out);
        Ok(())
    }

    async fn release(&self, _: C) -> anyhow::Result<()> {
        self.lines().map(LineDebugger::release);
        Ok(())
    }
}

fn line_stop(stop: witgraph_runtime::LineStop) -> lines::LineStop {
    lines::LineStop {
        reason: match stop.reason {
            StopReason::Breakpoint => lines::StopReason::Breakpoint,
            StopReason::Pause => lines::StopReason::Pause,
            StopReason::Step => lines::StopReason::Step,
        },
        file: stop.file,
        line: stop.line,
        column: stop.column,
        node: stop.node.map(|node| node.to_string()),
        frames: stop
            .frames
            .into_iter()
            .map(|frame| lines::StackFrame {
                function: frame.function,
                file: frame.file,
                line: frame.line,
            })
            .collect(),
        variables: stop
            .variables
            .into_iter()
            .map(|variable| {
                let mut children = Vec::new();
                flatten(&variable.name, &variable.children, &mut children);
                lines::Variable {
                    name: variable.name,
                    type_name: variable.type_name,
                    value: variable.value,
                    children,
                }
            })
            .collect(),
    }
}

/// A variable's fields and items, each with the path to it.
fn flatten(path: &str, children: &[witgraph_runtime::SourceVariable], out: &mut Vec<lines::Child>) {
    for child in children {
        let path = format!("{path}.{}", child.name);
        out.push(lines::Child {
            path: path.clone(),
            type_name: child.type_name.clone(),
            value: child.value.clone(),
        });
        flatten(&path, &child.children, out);
    }
}

/// Where a [`Debuggee`] is served, and what a debugger needs to attach.
pub struct Served {
    /// The URL to attach to, with the token in it.
    pub url: String,
    /// The SHA-256 of the server's certificate, which a browser pins
    /// (`serverCertificateHashes`): it is self-signed, and valid for 14 days.
    pub certificate_hash: [u8; 32],
    /// What a debugger must present, in the URL's `token` parameter.
    pub token: String,
    /// Serves until dropped, or until it fails.
    pub task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// Serves `debuggee` on `addr`, to debuggers presenting `token`.
///
/// # Errors
///
/// If the certificate cannot be made, or the address bound.
pub async fn serve<M: RuntimeMode, H: Host>(
    debuggee: Debuggee<M, H>,
    addr: SocketAddr,
    token: String,
) -> anyhow::Result<Served> {
    let names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        addr.ip().to_string(),
    ];
    let mut params =
        rcgen::CertificateParams::new(names).context("failed to describe the certificate")?;
    // Browsers pin a certificate by its hash only if it is valid for at most 14 days.
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = params.not_before + time::Duration::days(14);
    let key = rcgen::KeyPair::generate().context("failed to make a key")?;
    let cert = params
        .self_signed(&key)
        .context("failed to make a certificate")?;
    let cert = rustls::pki_types::CertificateDer::from(cert);
    let provider = web_transport_quinn::crypto::default_provider();
    let hash = web_transport_quinn::crypto::sha256(&provider, &cert);
    let mut certificate_hash = [0; 32];
    certificate_hash.copy_from_slice(hash.as_ref());

    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("failed to choose TLS 1.3")?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .context("failed to configure TLS")?;
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    let tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("failed to configure QUIC's TLS")?;
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(3)));
    // A debugger gone without closing (a tab closed, say) is let go of soon after, and what it
    // held with it.
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(10))
            .context("failed to set an idle timeout")?,
    ));
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(tls));
    config.transport_config(Arc::new(transport));
    let endpoint =
        quinn::Endpoint::server(config, addr).with_context(|| format!("failed to bind {addr}"))?;
    let bound = endpoint
        .local_addr()
        .context("failed to read the bound address")?;

    let server = Arc::new(wrpc_webtransport::Server::new());
    let calls = handle(debuggee.clone(), server.as_ref()).await?;

    let host = match bound.ip() {
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    let url = format!("https://{host}:{}/?token={token}", bound.port());
    let task = tokio::spawn(run(endpoint, server, calls, token.clone(), debuggee));
    Ok(Served {
        url,
        certificate_hash,
        token,
        task,
    })
}

/// Answers the debug protocol's calls for `debuggee` that `server` takes: a wRPC server on any
/// transport. [`serve`] does so on a WebTransport endpoint of its own; a program serving one
/// itself, beside its web UI say, does so with this, and tells `debuggee` when a debugger goes
/// ([`Debuggee::debugger_gone`]).
///
/// Gives what answers them, to run for as long as they are to be answered.
///
/// # Errors
///
/// If `server` cannot take the protocol's calls.
pub async fn handle<S, M, H>(
    debuggee: Debuggee<M, H>,
    server: &S,
) -> anyhow::Result<impl Future<Output = ()> + Send + 'static>
where
    S: wrpc_transport::Serve,
    M: RuntimeMode,
    H: Host,
{
    let invocations = bindings::serve(server, Handler { debuggee })
        .await
        .context("failed to serve the debugger's calls")?;
    let mut invocations = select_all(invocations.into_iter().map(|(_, _, calls)| calls));
    Ok(async move {
        let mut calls = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(call) = invocations.next() => {
                    if let Ok(call) = call {
                        calls.spawn(call);
                    }
                }
                Some(_) = calls.join_next() => {}
                else => return,
            }
        }
    })
}

async fn run<M: RuntimeMode, H: Host>(
    endpoint: quinn::Endpoint,
    server: Arc<wrpc_webtransport::Server>,
    calls: impl Future<Output = ()> + Send + 'static,
    token: String,
    debuggee: Debuggee<M, H>,
) -> anyhow::Result<()> {
    let calls = tokio::spawn(calls);
    let mut sessions = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            Some(connecting) = endpoint.accept() => {
                let (server, token) = (server.clone(), token.clone());
                let debuggee = debuggee.clone();
                sessions.spawn(async move {
                    let attached = session(connecting, server, token).await;
                    // Whatever ended it, a debugger gone holds nothing and stops nothing.
                    if matches!(attached, Ok(true) | Err(_)) {
                        debuggee.debugger_gone();
                    }
                    attached
                });
            }
            Some(_) = sessions.join_next() => {}
            else => break Ok(()),
        }
    };
    calls.abort();
    result
}

/// Serves one debugger, if it presents the token: says whether it did, once it has gone.
async fn session(
    connecting: quinn::Incoming,
    server: Arc<wrpc_webtransport::Server>,
    token: String,
) -> anyhow::Result<bool> {
    let connection = connecting.await.context("failed to accept QUIC")?;
    let request = web_transport_quinn::Request::accept(connection)
        .await
        .context("failed to accept WebTransport")?;
    let presented = request
        .url
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned());
    if presented.as_deref() != Some(token.as_str()) {
        request
            .reject(web_transport_quinn::http::StatusCode::FORBIDDEN)
            .await
            .context("failed to refuse a debugger")?;
        return Ok(false);
    }
    let session = request.ok().await.context("failed to open the session")?;
    // Until it goes, which is how a debugger's session ends.
    while let Ok((send, receive)) = session.accept_bi().await {
        server
            .accept((), send.into(), receive.into())
            .await
            .context("failed to take a call")?;
    }
    Ok(true)
}

#[cfg(test)]
mod protocol_tests {
    use super::PROTOCOL;

    /// FNV-1a: stable across Rust versions, as the standard hasher is not.
    fn fnv1a(text: &str) -> u64 {
        text.bytes()
            .filter(|byte| *byte != b'\r')
            .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            })
    }

    #[test]
    fn the_protocol_moves_on_with_its_wit() {
        let wit = include_str!("../wit/debug.wit");
        assert_eq!(
            (PROTOCOL, fnv1a(wit)),
            (4, 0x73b9_6b16_538b_e724),
            "`wit/debug.wit` changed: move `PROTOCOL` on, here and as `PROTOCOL` in \
             `packages/witgraph-loader/src/attach/session.ts`, and record the new hash here"
        );
    }
}
