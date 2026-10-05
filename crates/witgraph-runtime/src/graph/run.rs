//! Driving a loaded graph: ticks, the start rule, generations, events,
//! delivery, faults and stops.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::FutureExt;
use futures::future::Either;
use futures::stream::StreamExt;
use futures::task::{ArcWake, AtomicWaker};
use wasmtime::component::Val;
use witgraph_ir::{ConnectionId, NodeId, PortName, PortRef};

use super::{Route, RuntimeGraph};
use crate::engine::{self, Generation, Host, IslandEvent, IslandEventKind, NodeBinary, Outcome};
use crate::error::NodeFault;
use crate::island::{self, IslandState, StartInputs, StopCause, Work};
use crate::mode::RuntimeMode;
use crate::schedule::{FaultReport, TickResult};

/// The running islands whose generations asked to be polled again, so a
/// wake polls those islands only, each once.
pub(super) struct WakeSet {
    queued: Vec<AtomicBool>,
    ready: Mutex<VecDeque<usize>>,
    tick: AtomicWaker,
}

impl WakeSet {
    pub(super) fn new(islands: usize) -> Self {
        Self {
            queued: (0..islands).map(|_| AtomicBool::new(false)).collect(),
            ready: Mutex::new(VecDeque::new()),
            tick: AtomicWaker::new(),
        }
    }

    /// Queues `island` for polling (once, however often it is woken) and
    /// wakes the tick.
    pub(super) fn wake(&self, island: usize) {
        if let Some(queued) = self.queued.get(island)
            && !queued.swap(true, Ordering::AcqRel)
            && let Ok(mut ready) = self.ready.lock()
        {
            ready.push_back(island);
        }
        self.tick.wake();
    }

    /// How many islands are queued.
    fn len(&self) -> usize {
        self.ready.lock().map_or(0, |ready| ready.len())
    }

    /// Takes the next queued island. It may be woken (and queued) again
    /// while it is polled.
    fn pop(&self) -> Option<usize> {
        let island = self.ready.lock().ok()?.pop_front()?;
        if let Some(queued) = self.queued.get(island) {
            queued.store(false, Ordering::Release);
        }
        Some(island)
    }
}

/// The waker of one island's generation.
pub(super) struct IslandWaker {
    pub(super) island: usize,
    pub(super) set: Arc<WakeSet>,
}

impl ArcWake for IslandWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.set.wake(arc_self.island);
    }
}

/// What the tick loop woke up for.
enum Wake<D: 'static> {
    Event(IslandEvent),
    Finished(usize, Outcome<D>),
}

/// Ends a tick that did not run to its end (it was dropped, say) the way
/// [`RuntimeGraph::tick_until`] ends an interrupted one.
struct TickGuard<'a, M: RuntimeMode, H: Host> {
    rt: &'a mut RuntimeGraph<M, H>,
    finished: bool,
}

impl<M: RuntimeMode, H: Host> Drop for TickGuard<'_, M, H> {
    fn drop(&mut self) {
        if !self.finished {
            self.rt.tick_interrupted();
        }
    }
}

/// What a connection delivers to its input: the value itself, or, when the
/// connection unwraps an option, its payload ([`engine::option_payload`]).
fn delivered(val: &Arc<Val>, unwrap_option: bool) -> Option<Arc<Val>> {
    match (unwrap_option, &**val) {
        (true, Val::Option(_)) => engine::option_payload(val).map(|p| Arc::new(p.clone())),
        _ => Some(val.clone()),
    }
}

/// What a start pass did.
enum Started {
    Nothing,
    Some,
    /// It hit `max_steps_per_tick` with islands still waiting to start.
    LimitReached,
}

impl<M: RuntimeMode, H: Host> RuntimeGraph<M, H> {
    /// Runs one iteration of the graph: until it is quiescent, then latches
    /// feedback connections.
    ///
    /// Starts every island that can start, drives all in-flight
    /// generations concurrently, delivers Value outputs as `run`s return,
    /// and keeps starting islands whose inputs changed, until nothing is in
    /// flight and nothing can start. Then each feedback connection's latest
    /// value is written to its target (one loop iteration). A tick that
    /// returns [`TickResult::StepLimitReached`] or [`TickResult::Aborted`]
    /// latches nothing; a later tick finishes the iteration. Faults other
    /// than `fatal` do not end the tick: see
    /// [`take_faults`](Self::take_faults).
    ///
    /// An island whose generation never finishes (an endless stream, say)
    /// keeps the tick running: drive such a graph with
    /// [`tick_until`](Self::tick_until), or [`cancel`](Self::cancel) the
    /// node. Dropping the tick's future is safe and ends it as
    /// `tick_until` ends an interrupted one: in-flight generations persist
    /// and resume on the next tick.
    pub async fn tick(&mut self) -> TickResult {
        self.tick_until(std::future::pending()).await
    }

    /// Like [`tick`](Self::tick), but ends early when `stop` completes (a
    /// timer, say), with [`TickResult::Interrupted`]. In-flight generations
    /// carry on with the next tick. Every buffered feedback value whose
    /// iteration is over is latched as the tick ends: the value's target
    /// island has settled (it is not running, cannot start, and nothing
    /// upstream of it can), and so has every feedback source into it. So a
    /// feedback loop advances one iteration per tick even beside an island
    /// that never finishes.
    pub async fn tick_until(&mut self, stop: impl Future<Output = ()>) -> TickResult {
        let mut guard = TickGuard {
            rt: self,
            finished: false,
        };
        let finished = {
            let run = guard.rt.run_tick();
            futures::pin_mut!(run, stop);
            match futures::future::select(run, stop).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(((), _)) => None,
            }
        };
        guard.finished = true;
        match finished {
            Some(result) => result,
            None => {
                guard.rt.tick_interrupted();
                TickResult::Interrupted
            }
        }
    }

    /// The body of a tick.
    async fn run_tick(&mut self) -> TickResult {
        let mut steps = 0usize;
        let mut progressed = false;
        loop {
            progressed |= self.drain_events();
            match self.start_ready(&mut steps) {
                Started::Nothing => {}
                Started::Some => progressed = true,
                Started::LimitReached => return TickResult::StepLimitReached,
            }
            if self.running == 0 {
                break;
            }
            match self.next_wake().await {
                Wake::Event(event) => progressed |= self.handle_event(event),
                Wake::Finished(index, outcome) => {
                    // Events sent before the generation finished come first.
                    self.drain_events();
                    progressed = true;
                    if let Some(abort) = self.handle_finished(index, outcome) {
                        return abort;
                    }
                }
            }
        }
        progressed |= self.latch_feedback(|_| true);
        if progressed {
            TickResult::Progress
        } else {
            TickResult::Idle
        }
    }

    /// Ends a tick that stopped short of quiescence (dropped or
    /// interrupted): handles the events its generations sent, and latches
    /// the feedback of every iteration that is over (see
    /// [`tick_until`](Self::tick_until)).
    pub(super) fn tick_interrupted(&mut self) {
        self.drain_events();
        if self.feedback.is_empty() {
            return;
        }
        let (blocked, unsettled) = self.unsettled();
        let settled = |island: usize| {
            let slot = &self.slots[island];
            !slot.state.is_running()
                && !self.next_ready(island)
                && !blocked[island]
                && slot
                    .feedback_sources
                    .iter()
                    .all(|&source| !unsettled[source])
        };
        let latch: HashSet<ConnectionId> = self
            .feedback
            .iter()
            .filter(|(_, (to, _, _))| self.compiled.island_of(&to.node).is_some_and(settled))
            .map(|(conn, _)| conn.clone())
            .collect();
        self.latch_feedback(|conn| latch.contains(conn));
    }

    /// Applies an island transition, keeps the count of running islands,
    /// and reports the resulting node-phase change for every member.
    pub(super) fn transition(
        &mut self,
        index: usize,
        step: impl FnOnce(IslandState<H::Data>) -> IslandState<H::Data>,
    ) {
        let state = std::mem::replace(&mut self.slots[index].state, IslandState::placeholder());
        let (from, was_running) = (state.node_phase(), state.is_running());
        let next = step(state);
        let (to, is_running) = (next.node_phase(), next.is_running());
        self.slots[index].state = next;
        match (was_running, is_running) {
            (false, true) => self.running += 1,
            (true, false) => self.running -= 1,
            _ => {}
        }
        if from != to {
            for member in &self.slots[index].plan.members {
                self.mode.on_phase_transition(&member.node, from, to);
            }
        }
    }

    /// Records a host write that changed an input: feedback buffered for
    /// it is dropped, and feedback into it from a generation in flight (or
    /// a replay owed) is stale, never overwriting it.
    pub(super) fn note_host_write(&mut self, port: &PortRef) {
        self.feedback.retain(|_, (to, _, _)| to != port);
        let Some(into) = self.feedback_into.get(port) else {
            return;
        };
        for (conn, source) in into {
            let slot = &mut self.slots[*source];
            if slot.state.is_running() {
                slot.stale_feedback.insert(conn.clone());
            }
            if let Some(replay) = slot.owed.replay_mut() {
                replay.stale.insert(conn.clone());
            }
        }
    }

    /// Forgets a Value input's latched value. Returns whether it had one
    /// (and so made the island owe a generation).
    pub(super) fn clear_input_value(&mut self, port: PortRef) -> bool {
        if self.inputs.remove(&port).is_none() {
            return false;
        }
        self.dirty = true;
        if let Some(index) = self.compiled.island_of(&port.node) {
            self.slots[index].owed.push_latched();
        }
        true
    }

    /// Writes a delivered Value into an input from outside its island:
    /// unwrapped first when the connection unwraps an option, where `none`
    /// clears the input. Returns whether the input changed.
    fn write_input(&mut self, port: PortRef, val: &Arc<Val>, unwrap_option: bool) -> bool {
        match delivered(val, unwrap_option) {
            Some(val) => self.set_input(port, val),
            None => self.clear_input_value(port),
        }
    }

    /// Writes a Value input from outside its island. Returns whether the
    /// value changed (and so made the island owe a generation).
    pub(super) fn set_input(&mut self, port: PortRef, val: Arc<Val>) -> bool {
        if self.inputs.get(&port).is_some_and(|old| **old == *val) {
            return false;
        }
        self.dirty = true;
        if let Some(index) = self.compiled.island_of(&port.node) {
            self.slots[index].owed.push_latched();
        }
        self.inputs.insert(port, val);
        true
    }

    /// Whether the island's next owed generation has what it needs to
    /// start, ignoring its phase, what is upstream of it, and resources. A
    /// replay brings its own inputs; a run on the latched inputs needs
    /// every required one to have a value.
    pub(super) fn next_ready(&self, index: usize) -> bool {
        let slot = &self.slots[index];
        if slot.owed.next_is_replay() {
            return true;
        }
        slot.owed.has_latched() && slot.required.iter().all(|p| self.inputs.contains_key(p))
    }

    /// Per island, whether something upstream of it can still change its
    /// inputs, so it must not start yet; and per node (graph-wide index),
    /// whether it is *unsettled*: it can still produce new outputs.
    ///
    /// In a running island, a member is unsettled while its `run` is still
    /// to return, or while one of its writers is unsettled. An idle or
    /// stopped island runs all of its members together, so all of them
    /// are unsettled while it could start, or while it has an unsettled
    /// writer in another island. An island waits while any of its members
    /// has an unsettled writer in another island. A running island that
    /// owes another generation holds back nothing beyond the above: it may
    /// never finish (an endless stream), and what it delivers next is
    /// handled by change detection. The islands form a DAG, so an island
    /// never waits on itself. Linear in the size of the graph.
    pub(super) fn unsettled(&self) -> (Vec<bool>, Vec<bool>) {
        let mut unsettled = vec![false; self.slots.iter().map(|s| s.awaiting.len()).sum()];
        let mut blocked = vec![false; self.slots.len()];
        // Islands are in topological order, and so are each island's
        // members: every writer is decided before what it writes.
        for (island, slot) in self.slots.iter().enumerate() {
            let own = slot.offset..slot.offset + slot.awaiting.len();
            blocked[island] = slot
                .preds
                .iter()
                .flatten()
                .any(|&pred| !own.contains(&pred) && unsettled[pred]);
            if slot.state.is_running() {
                for member in 0..slot.awaiting.len() {
                    let upstream = slot.preds[member].iter().any(|&pred| unsettled[pred]);
                    unsettled[slot.offset + member] = slot.awaiting[member] || upstream;
                }
            } else {
                let whole = blocked[island] || self.next_ready(island);
                unsettled[own].fill(whole);
            }
        }
        (blocked, unsettled)
    }

    /// Starts every island that can start, in island (topological) order,
    /// in one pass: starting an island never lets another start. Each
    /// generation started counts as a step.
    fn start_ready(&mut self, steps: &mut usize) -> Started {
        if !self.dirty {
            return Started::Nothing;
        }
        self.dirty = false;
        // Cheap before the graph-wide pass: does any island owe work it
        // could start?
        let waiting = self
            .slots
            .iter()
            .any(|slot| !slot.state.is_running() && !slot.owed.is_empty());
        if !waiting {
            return Started::Nothing;
        }
        let (blocked, _) = self.unsettled();
        let mut started = Started::Nothing;
        for (index, blocked) in blocked.into_iter().enumerate() {
            if blocked
                || self.slots[index].state.is_running()
                || !self.next_ready(index)
                || !self.resources.can_acquire(index)
            {
                continue;
            }
            if *steps >= self.config.max_steps_per_tick {
                // Islands are still waiting to start.
                self.dirty = true;
                return Started::LimitReached;
            }
            started = Started::Some;
            *steps += 1;
            self.start_generation(index);
        }
        started
    }

    /// Starts the island's next owed generation. A stopped island's
    /// generation first rebuilds its Store, inside the generation's future
    /// (see [`engine::rebuild_and_run`]).
    pub(super) fn start_generation(&mut self, index: usize) {
        // Decided before any side effect: only an idle or stopped island
        // with work owed starts.
        let stopped = match &self.slots[index].state {
            IslandState::Idle(_) => false,
            IslandState::Stopped(_) => true,
            IslandState::Running(_) => return,
        };
        let Some(work) = self.slots[index].owed.take_next() else {
            return;
        };
        let (started_with, stale) = match work {
            Work::Replay(replay) => (replay.inputs, replay.stale),
            Work::Latched => (self.external_inputs(index), BTreeSet::new()),
        };
        let external: Vec<Vec<Option<Val>>> = started_with
            .iter()
            .map(|fields| fields.iter().map(|v| v.as_deref().cloned()).collect())
            .collect();
        let slot = &self.slots[index];
        let plan = slot.plan.clone();
        let settings = self.settings;
        let events = self.events_tx.clone();
        let generation = |number| Generation {
            plan,
            external,
            settings,
            number,
            events,
        };
        let mut number = 0;
        if stopped {
            let members: Vec<NodeId> = slot.plan.members.iter().map(|m| m.node.clone()).collect();
            let binaries: Vec<(NodeId, Arc<NodeBinary<H::Data>>)> = members
                .iter()
                .cloned()
                .zip(slot.binaries.iter().cloned())
                .collect();
            let state = engine::HostState::new(self.config.max_island_memory, members.len());
            let data = self.host.island_data(&members, state);
            let engine = self.engine.clone();
            self.transition(index, |state| match state {
                IslandState::Stopped(island) => island
                    .start_rebuilt(started_with, |n| {
                        number = n;
                        engine::rebuild_and_run(engine, binaries, data, generation(n)).boxed()
                    })
                    .into(),
                other => island::misuse(other, "Stopped"),
            });
        } else {
            self.transition(index, |state| match state {
                IslandState::Idle(island) => island
                    .start(started_with, |store, n| {
                        number = n;
                        engine::run_generation(store, generation(n)).boxed()
                    })
                    .into(),
                other => island::misuse(other, "Idle"),
            });
        }
        let slot = &mut self.slots[index];
        slot.awaiting.fill(true);
        slot.stale_feedback = stale;
        self.resources.acquire(index);
        self.wakes.wake(index);
        self.mode.on_generation_started(index, number);
    }

    /// The latched external Value inputs of each member, per field.
    fn external_inputs(&self, index: usize) -> StartInputs {
        self.slots[index]
            .plan
            .members
            .iter()
            .map(|member| {
                let mut fields = vec![None; member.field_count()];
                for (field, input) in member.external_values() {
                    fields[field] = self.inputs.get(&input.port).cloned();
                }
                fields
            })
            .collect()
    }

    /// Waits for the next island event or finished generation, polling
    /// only the islands whose generations asked to be polled. A generation
    /// lives in its island's state, so dropping this future loses nothing.
    async fn next_wake(&mut self) -> Wake<H::Data> {
        let Self {
            slots,
            events_rx,
            wakes,
            ..
        } = self;
        poll_fn(|cx| {
            wakes.tick.register(cx.waker());
            if let Poll::Ready(Some(event)) = events_rx.poll_next_unpin(cx) {
                return Poll::Ready(Wake::Event(event));
            }
            // Only the islands queued now: one that wakes itself while it is
            // polled (yielding after its fuel interval, say) waits for the
            // next poll, so the executor gets control back.
            for _ in 0..wakes.len() {
                let Some(index) = wakes.pop() else {
                    break;
                };
                let Some(slot) = slots.get_mut(index) else {
                    continue;
                };
                if let IslandState::Running(island) = &mut slot.state {
                    let mut island_cx = Context::from_waker(&slot.waker);
                    if let Poll::Ready(outcome) = island.poll(&mut island_cx) {
                        return Poll::Ready(Wake::Finished(index, outcome));
                    }
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Handles every event already queued. Returns whether any belonged
    /// to a current generation.
    pub(super) fn drain_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.events_rx.try_recv() {
            any |= self.handle_event(event);
        }
        any
    }

    /// Handles one event. Returns `false` for a stale one (from a dropped
    /// generation).
    fn handle_event(&mut self, event: IslandEvent) -> bool {
        let current = self
            .slots
            .get(event.island)
            .and_then(|slot| slot.state.running_generation());
        if current != Some(event.generation) {
            return false;
        }
        let plan = self.slots[event.island].plan.clone();
        match event.kind {
            IslandEventKind::RunStarted { member } => {
                if let Some(member) = plan.members.get(member) {
                    self.mode.on_run_started(&member.node);
                }
            }
            IslandEventKind::Rebuilt => {
                for member in &plan.members {
                    self.mode.on_restarted(&member.node);
                }
            }
            IslandEventKind::RunReturned { member, values } => {
                if let Some(awaiting) = self.slots[event.island].awaiting.get_mut(member) {
                    *awaiting = false;
                }
                // What waited on this member may start now.
                self.dirty = true;
                if let Some(member) = plan.members.get(member) {
                    self.mode.on_run_returned(&member.node);
                    self.deliver(event.island, &member.node, values);
                }
            }
        }
        true
    }

    /// Latches a returned `run`'s Value outputs and routes them. Each value
    /// is shared, not copied, between the outputs, the inputs it reaches
    /// and buffered feedback. Guest values are already canonical.
    fn deliver(&mut self, island: usize, node: &NodeId, values: Vec<(PortName, Val)>) {
        for (port, val) in values {
            let output = PortRef::new(node.clone(), port);
            let val = Arc::new(val);
            if let Some(edges) = self.out_edges.get(&output).cloned() {
                for edge in edges.iter() {
                    match edge.route {
                        // Computed from inputs older than a host write to
                        // its target: the host's value stands.
                        Route::Feedback => {
                            if !self.slots[island].stale_feedback.contains(&edge.id) {
                                self.feedback.insert(
                                    edge.id.clone(),
                                    (edge.to.clone(), edge.unwrap_option, val.clone()),
                                );
                            }
                        }
                        // Consumed inside this generation; recorded, not a
                        // change.
                        Route::Internal => match delivered(&val, edge.unwrap_option) {
                            Some(val) => {
                                self.inputs.insert(edge.to.clone(), val);
                            }
                            None => {
                                self.inputs.remove(&edge.to);
                            }
                        },
                        Route::External => {
                            self.write_input(edge.to.clone(), &val, edge.unwrap_option);
                        }
                    }
                }
            }
            self.outputs.insert(output, val);
        }
    }

    /// Handles a running island's finished generation. Returns the tick's
    /// result when a `fatal` call aborts it. Work the island owes survives
    /// a fault: if an input changed while the failing generation ran, the
    /// island is rebuilt and runs again.
    fn handle_finished(&mut self, index: usize, outcome: Outcome<H::Data>) -> Option<TickResult> {
        let generation = self.slots[index].state.running_generation().unwrap_or(0);
        self.resources.release(index);
        self.dirty = true;
        match outcome {
            Ok(store) => {
                self.slots[index].awaiting.fill(false);
                self.transition(index, |state| state.finish(store));
                self.mode.on_generation_finished(index, generation);
                None
            }
            Err((fault, caller)) => {
                let members = &self.slots[index].plan.members;
                // The member that called `fatal` or failed to rebuild, or
                // else the island's only member. A trap in a multi-member
                // island cannot be pinned on one: a member that returned
                // may still be running a task it spawned.
                let culprit = caller.or_else(|| match members.as_slice() {
                    [only] => Some(only.node.clone()),
                    _ => None,
                });
                let abort = matches!(fault, NodeFault::Fatal { .. }).then(|| {
                    let node = culprit
                        .clone()
                        .or_else(|| members.first().map(|m| m.node.clone()))
                        .unwrap_or_else(|| NodeId::from("?"));
                    TickResult::Aborted {
                        node,
                        fault: fault.clone(),
                    }
                });
                self.mode.on_generation_stopped(index, generation);
                self.fault_island(index, fault, culprit);
                abort
            }
        }
    }

    /// Stops the island with a fault, reports it for every member, and
    /// records it for [`take_faults`](Self::take_faults).
    fn fault_island(&mut self, index: usize, fault: NodeFault, culprit: Option<NodeId>) {
        self.slots[index].awaiting.fill(false);
        let cause = StopCause::Faulted(fault.clone(), culprit.clone());
        self.transition(index, |state| state.stop(cause));
        let plan = self.slots[index].plan.clone();
        for member in &plan.members {
            self.mode.on_node_fault(&member.node, &fault);
        }
        self.faults.insert(
            index,
            FaultReport {
                members: plan.members.iter().map(|m| m.node.clone()).collect(),
                culprit,
                fault,
            },
        );
    }

    /// Stops an island because of the host. Its owed work is forgotten,
    /// feedback values buffered for its members included. A live, restored
    /// or (on cancel) faulted island then has its generation and Store
    /// dropped, its resources released, and every member reported
    /// cancelled. An island already cancelled or shut down, or a faulted
    /// one on shutdown, keeps its phase and reports nothing. Either way,
    /// islands it was holding back may start now.
    pub(super) fn stop_island(&mut self, index: usize, cause: StopCause) {
        self.dirty = true;
        self.slots[index].owed.clear();
        let compiled = &self.compiled;
        self.feedback
            .retain(|_, (to, _, _)| compiled.island_of(&to.node) != Some(index));
        let keep = match self.slots[index].state.stop_cause() {
            Some(StopCause::Cancelled | StopCause::Shutdown) => true,
            Some(StopCause::Faulted(..)) => matches!(cause, StopCause::Shutdown),
            Some(StopCause::Restored) | None => false,
        };
        if keep {
            return;
        }
        let generation = self.slots[index].state.running_generation();
        self.slots[index].awaiting.fill(false);
        self.transition(index, |state| state.stop(cause));
        self.resources.release(index);
        if let Some(generation) = generation {
            self.mode.on_generation_stopped(index, generation);
        }
        let plan = self.slots[index].plan.clone();
        for member in &plan.members {
            self.mode.on_cancelled(&member.node);
        }
    }

    /// Writes the buffered feedback values `latch` picks to their targets,
    /// keeping the others buffered. Returns whether any input changed.
    pub(super) fn latch_feedback(&mut self, mut latch: impl FnMut(&ConnectionId) -> bool) -> bool {
        let mut changed = false;
        for (conn, (to, unwrap_option, val)) in std::mem::take(&mut self.feedback) {
            if latch(&conn) {
                changed |= self.write_input(to, &val, unwrap_option);
            } else {
                self.feedback.insert(conn, (to, unwrap_option, val));
            }
        }
        changed
    }
}
