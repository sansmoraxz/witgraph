//! Stopping a guest on a line of its source, on wasmtime: [`RuntimeLines`], behind the `debug`
//! feature, for a graph loaded with [`RuntimeConfig::debug_lines`](crate::RuntimeConfig).
//!
//! Wasmtime sets breakpoints in a Store's code itself, and calls a handler with the guest still
//! on the stack, which then holds it by not returning. That needs the Store, which a running
//! guest has. So what a host asks is noted here, and the engine's epoch moved on: the guest
//! notices at its next call or loop, even one that never returns, and the handler is called
//! with the Store, which does what was asked.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};

use futures::channel::oneshot;
use wasmtime::{
    AsContext, AsContextMut, DebugEvent, DebugHandler, Engine, FrameHandle, Instance, Module,
    ModulePC, Store, StoreContextMut, UpdateDeadline, Val,
};
use witgraph_ir::NodeId;
use witgraph_probes::{
    Frame, LeavingLine, LineIndex, ModuleDebug, Passing, ProbeId, StepKind, core_modules,
    defined_function_names, imported_functions, source_lines,
};
use witgraph_traits::executor::MaybeSend;
use witgraph_traits::lines::{LineDebugger, LineStop, OwnLines, StackFrame, StopReason};

/// What tells one core module from another, for finding its nodes.
fn fingerprint(module: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    module.hash(&mut hasher);
    hasher.finish()
}

/// By node, in the order they are instantiated, the core modules of its component.
pub(crate) fn node_modules<'a>(
    nodes: impl Iterator<Item = (&'a NodeId, &'a [u8])>,
) -> Vec<(NodeId, Vec<u64>)> {
    nodes
        .map(|(node, bytes)| {
            let modules = core_modules(bytes).unwrap_or_default();
            (node.clone(), modules.into_iter().map(fingerprint).collect())
        })
        .collect()
}

/// A core module with lines to stop on.
struct ModuleLines {
    module: Module,
    index: LineIndex,
    debug: Option<ModuleDebug>,
    names: HashMap<u32, String>,
    /// How many functions it imports: a defined function's index comes after those.
    imported: u32,
    /// The node of each of its instances, in order.
    nodes: Vec<NodeId>,
}

/// A step under way: it stays with the guest it started in.
#[derive(Clone, Copy)]
struct Step {
    how: StepKind,
    module: usize,
    instance: Instance,
    from: ProbeId,
    depth: usize,
}

/// What a stop has to say besides what the host is told.
#[derive(Clone, Copy)]
struct StoppedAt {
    module: usize,
    instance: Instance,
    probe: ProbeId,
    depth: usize,
}

#[derive(Default)]
struct State {
    modules: Vec<Arc<ModuleLines>>,
    /// What a host asked, by file: the line asked for, and the one it came to.
    asked: HashMap<String, HashMap<u32, u32>>,
    pause: bool,
    step: Option<Step>,
    stopped: Option<LineStop>,
    stopped_at: Option<StoppedAt>,
    /// What lets the guest stopped go on.
    go: Option<oneshot::Sender<()>>,
    /// Stops the host has not been told of.
    unseen: VecDeque<LineStop>,
    wakers: Vec<Waker>,
    /// What the live Store has set.
    set: BTreeSet<(usize, ProbeId)>,
    single_stepping: bool,
    /// Guests gone on from a line that would stop them again before they have left it.
    leaving: Vec<Leaving>,
}

/// A guest on its way off a line.
struct Leaving {
    module: usize,
    instance: Instance,
    line: LeavingLine,
}

impl State {
    fn wanted(&self) -> BTreeSet<(usize, ProbeId)> {
        let mut wanted = BTreeSet::new();
        for (file, asked) in &self.asked {
            for line in asked.values() {
                for (at, module) in self.modules.iter().enumerate() {
                    for site in module.index.sites(file, *line) {
                        wanted.insert((at, *site));
                    }
                }
            }
        }
        wanted
    }
}

struct Shared {
    engine: Engine,
    nodes: Vec<(NodeId, Vec<u64>)>,
    own: OwnLines,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gets the Store's attention, wherever its guest is.
    fn notify_store(&self) {
        self.engine.increment_epoch();
    }

    /// Makes what the Store has set what is wanted.
    fn sync<D: 'static>(&self, store: &mut StoreContextMut<'_, D>) {
        let (modules, adding, removing, single, kept) = {
            let mut state = self.state();
            let wanted = state.wanted();
            let adding: Vec<_> = wanted.difference(&state.set).copied().collect();
            let removing: Vec<_> = state.set.difference(&wanted).copied().collect();
            let State {
                leaving, modules, ..
            } = &mut *state;
            leaving.retain(|leaving| {
                modules.get(leaving.module).is_some_and(|known| {
                    known.index.still_armed(&leaving.line, |site| {
                        wanted.contains(&(leaving.module, site))
                    })
                })
            });
            // Watched at every line, to see it leave, which is over quickly.
            let single = state.pause || state.step.is_some() || !state.leaving.is_empty();
            let change = (single != state.single_stepping).then_some(single);
            state.set = wanted;
            state.single_stepping = single;
            let kept: Vec<_> = state.set.iter().copied().collect();
            (state.modules.clone(), adding, removing, change, kept)
        };
        if adding.is_empty() && removing.is_empty() && single.is_none() {
            return;
        }
        let Some(mut edit) = store.as_context_mut().edit_breakpoints() else {
            return;
        };
        let offset = |at: usize, probe: ProbeId| {
            let probe = modules.get(at)?.index.probe(probe)?;
            Some((&modules[at].module, ModulePC::new(probe.offset.0)))
        };
        for (at, probe) in adding {
            if let Some((module, pc)) = offset(at, probe) {
                let _ = edit.add_breakpoint(module, pc);
            }
        }
        for (at, probe) in removing {
            if let Some((module, pc)) = offset(at, probe) {
                let _ = edit.remove_breakpoint(module, pc);
            }
        }
        if let Some(on) = single {
            let _ = edit.single_step(on);
            // Ending single-stepping leaves the code unpatched for what is set: set them again.
            if !on {
                for (at, probe) in kept {
                    if let Some((module, pc)) = offset(at, probe) {
                        let _ = edit.remove_breakpoint(module, pc);
                        let _ = edit.add_breakpoint(module, pc);
                    }
                }
            }
        }
    }

    /// The frames of `module` on the stack from `top`, innermost first: each function, among
    /// those it defines, and where in the module it is.
    fn own_frames<D: 'static>(
        store: &mut StoreContextMut<'_, D>,
        top: &FrameHandle,
        module: u64,
    ) -> Vec<(u32, u32)> {
        let mut frames = Vec::new();
        let mut at = Some(top.clone());
        while let Some(frame) = at {
            let same = matches!(
                frame.module(store.as_context_mut()),
                Ok(Some(found)) if found.debug_index_in_engine() == module
            );
            if same && let Ok(Some((function, pc))) = frame.wasm_function_index_and_pc(&mut *store)
            {
                frames.push((function.as_u32(), pc.raw()));
            }
            at = frame.parent(&mut *store).ok().flatten();
        }
        frames
    }

    /// If the guest should stop where it is, says so to the host, and returns what it waits on.
    fn stop_here<D: 'static>(
        &self,
        store: &mut StoreContextMut<'_, D>,
    ) -> Option<oneshot::Receiver<()>> {
        let top = store.as_context_mut().debug_exit_frames().next()?;
        let (_, pc) = top.wasm_function_index_and_pc(&mut *store).ok()??;
        let module_id = top
            .module(store.as_context_mut())
            .ok()??
            .debug_index_in_engine();
        let (at, module, pause, mut armed, step) = {
            let state = self.state();
            let at = state
                .modules
                .iter()
                .position(|known| known.module.debug_index_in_engine() == module_id)?;
            let module = state.modules[at].clone();
            let probe = module.index.at(pc.raw())?;
            (
                at,
                module,
                state.pause,
                state.set.contains(&(at, probe)),
                state.step,
            )
        };
        let probe = module.index.at(pc.raw())?;
        let instance = top.instance(&mut *store).ok()?;
        let frames = Self::own_frames(store, &top, module_id);
        let depth = frames.len();
        {
            let mut state = self.state();
            let leaving = state
                .leaving
                .iter()
                .position(|leaving| leaving.module == at && leaving.instance == instance);
            if let Some(leaving) = leaving {
                match module
                    .index
                    .passing(&mut state.leaving[leaving].line, probe, depth)
                {
                    Passing::OnTheLine => armed = false,
                    Passing::InACall => {}
                    Passing::Left => {
                        state.leaving.remove(leaving);
                    }
                }
            }
        }
        let stepped = step.is_some_and(|step| {
            step.module == at
                && step.instance == instance
                && module
                    .index
                    .step_ends(step.how, step.from, step.depth, probe, depth)
        });
        let reason = if armed {
            StopReason::Breakpoint
        } else if pause {
            StopReason::Pause
        } else if stepped {
            StopReason::Step
        } else {
            return None;
        };

        let source = module.index.probe(probe)?;
        let ordinal = store
            .as_context_mut()
            .debug_all_instances()
            .into_iter()
            .filter(|other| other.module(store.as_context()).debug_index_in_engine() == module_id)
            .position(|other| other == instance);
        let stop = LineStop {
            reason,
            file: source.file.clone(),
            line: source.line,
            column: source.column,
            node: ordinal.and_then(|ordinal| module.nodes.get(ordinal).cloned()),
            frames: frames
                .iter()
                .map(|(function, pc)| {
                    let line = module.index.line_at(module.imported + function, *pc);
                    StackFrame {
                        function: module
                            .names
                            .get(function)
                            .cloned()
                            .unwrap_or_else(|| format!("function {function}")),
                        file: line.map(|probe| probe.file.clone()),
                        line: line.map(|probe| probe.line),
                    }
                })
                .collect(),
            variables: read_variables(store, &top, instance, &module, source.offset),
        };

        let (go, wait) = oneshot::channel();
        let mut state = self.state();
        state.pause = false;
        state.step = None;
        state.stopped = Some(stop.clone());
        state.stopped_at = Some(StoppedAt {
            module: at,
            instance,
            probe,
            depth,
        });
        state
            .leaving
            .retain(|leaving| leaving.module != at || leaving.instance != instance);
        let line = module
            .index
            .leaving_line(probe, depth, |site| state.set.contains(&(at, site)));
        if let Some(line) = line {
            state.leaving.push(Leaving {
                module: at,
                instance,
                line,
            });
        }
        state.go = Some(go);
        state.unseen.push_back(stop);
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
        Some(wait)
    }

    /// Lets the guest stopped go on.
    fn go_on(state: &mut State) {
        state.stopped = None;
        state.stopped_at = None;
        if let Some(go) = state.go.take() {
            let _ = go.send(());
        }
    }

    fn step(&self, how: StepKind) {
        let mut state = self.state();
        let Some(stopped) = state.stopped_at else {
            return;
        };
        state.step = Some(Step {
            how,
            module: stopped.module,
            instance: stopped.instance,
            from: stopped.probe,
            depth: stopped.depth,
        });
        Self::go_on(&mut state);
    }
}

/// What a guest had when it stopped.
struct StoppedFrame<'a> {
    locals: Vec<Option<[u8; 8]>>,
    globals: Vec<Option<[u8; 8]>>,
    memory: &'a [u8],
}

impl Frame for StoppedFrame<'_> {
    fn local(&self, index: u32) -> Option<[u8; 8]> {
        *self.locals.get(index as usize)?
    }

    fn global(&self, index: u32) -> Option<[u8; 8]> {
        *self.globals.get(index as usize)?
    }

    fn read(&self, address: u32, len: u32) -> Option<Vec<u8>> {
        let start = address as usize;
        let end = start.checked_add(len as usize)?;
        self.memory.get(start..end).map(<[u8]>::to_vec)
    }
}

/// A wasm value as its little-endian bytes, if it is a number.
fn value_bytes(value: &Val) -> Option<[u8; 8]> {
    let mut bytes = [0; 8];
    match value {
        Val::I32(value) => bytes[..4].copy_from_slice(&value.to_le_bytes()),
        Val::I64(value) => bytes.copy_from_slice(&value.to_le_bytes()),
        Val::F32(bits) => bytes[..4].copy_from_slice(&bits.to_le_bytes()),
        Val::F64(bits) => bytes.copy_from_slice(&bits.to_le_bytes()),
        _ => return None,
    }
    Some(bytes)
}

fn read_variables<D: 'static>(
    store: &mut StoreContextMut<'_, D>,
    top: &FrameHandle,
    instance: Instance,
    module: &ModuleLines,
    offset: witgraph_probes::ModuleOffset,
) -> Vec<witgraph_traits::lines::SourceVariable> {
    let Some(debug) = &module.debug else {
        return Vec::new();
    };
    let count = top.num_locals(&mut *store).unwrap_or(0);
    let locals: Vec<_> = (0..count)
        .map(|index| {
            let value = top.local(&mut *store, index).ok()?;
            value_bytes(&value)
        })
        .collect();
    let mut globals = Vec::new();
    while let Some(global) = instance.debug_global(
        &mut *store,
        u32::try_from(globals.len()).unwrap_or(u32::MAX),
    ) {
        globals.push(value_bytes(&global.get(&mut *store)));
    }
    let memory = instance.debug_memory(&mut *store, 0);
    let frame = StoppedFrame {
        locals,
        globals,
        memory: memory
            .as_ref()
            .map_or(&[], |memory| memory.data(store.as_context())),
    };
    debug.variables_at(offset, &frame).unwrap_or_default()
}

/// Calls [`Shared`] with the Store, when a guest gives it up.
struct Handler<D>(Arc<Shared>, PhantomData<fn() -> D>);

impl<D> Clone for Handler<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

/// Forgets a stop whose Store goes away while it waits, as a shutdown does.
struct ForgetStop(Arc<Shared>);

impl Drop for ForgetStop {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.stopped = None;
        state.stopped_at = None;
        state.go = None;
    }
}

impl<D: Send + 'static> DebugHandler for Handler<D> {
    type Data = D;

    fn handle(
        &self,
        mut store: StoreContextMut<'_, D>,
        event: DebugEvent<'_>,
    ) -> impl Future<Output = ()> + Send {
        let shared = self.0.clone();
        let at_a_line = matches!(event, DebugEvent::Breakpoint);
        async move {
            shared.sync(&mut store);
            if !at_a_line {
                return;
            }
            let Some(wait) = shared.stop_here(&mut store) else {
                return;
            };
            let _forget = ForgetStop(shared.clone());
            let _ = wait.await;
            shared.sync(&mut store);
        }
    }
}

/// Sets breakpoints in a graph's guests, and steps through them. Clone it to hold on to it while
/// a tick runs: [`RuntimeGraph::lines`](crate::RuntimeGraph::lines).
#[derive(Clone)]
pub struct RuntimeLines {
    shared: Arc<Shared>,
}

impl RuntimeLines {
    pub(crate) fn new(engine: Engine, nodes: Vec<(NodeId, Vec<u64>)>, own: OwnLines) -> Self {
        Self {
            shared: Arc::new(Shared {
                engine,
                nodes,
                own,
                state: Mutex::default(),
            }),
        }
    }

    /// Whether `engine` was made to debug: wasmtime panics at being asked otherwise.
    pub(crate) fn engine_can(engine: &Engine) -> bool {
        Store::new(engine, ()).edit_breakpoints().is_some()
    }

    /// Readies a new Store, before anything runs in it.
    pub(crate) fn attach<D: Send + 'static>(&self, store: &mut Store<D>) {
        store.set_debug_handler(Handler::<D>(self.shared.clone(), PhantomData));
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Continue(1)));
        let mut state = self.shared.state();
        state.set.clear();
        state.single_stepping = false;
        state.leaving.clear();
    }

    /// Learns what a new Store's modules have to say, and sets what is wanted there.
    pub(crate) fn arm<D: 'static>(&self, store: &mut Store<D>) {
        let modules = store.debug_all_modules();
        {
            let mut state = self.shared.state();
            for module in modules {
                let id = module.debug_index_in_engine();
                if state
                    .modules
                    .iter()
                    .any(|known| known.module.debug_index_in_engine() == id)
                {
                    continue;
                }
                let Some(bytes) = module.debug_bytecode() else {
                    continue;
                };
                let probes = source_lines(bytes, &self.shared.own).unwrap_or_default();
                if probes.is_empty() {
                    continue;
                }
                let print = fingerprint(bytes);
                state.modules.push(Arc::new(ModuleLines {
                    index: LineIndex::new(probes),
                    debug: ModuleDebug::new(bytes).ok().flatten(),
                    names: defined_function_names(bytes).unwrap_or_default(),
                    imported: imported_functions(bytes).unwrap_or(0),
                    nodes: self
                        .shared
                        .nodes
                        .iter()
                        .filter(|(_, modules)| modules.contains(&print))
                        .map(|(node, _)| node.clone())
                        .collect(),
                    module: module.clone(),
                }));
            }
        }
        self.shared.sync(&mut store.as_context_mut());
    }
}

impl LineDebugger for RuntimeLines {
    fn files(&self) -> Vec<String> {
        let state = self.shared.state();
        let mut files: Vec<String> = state
            .modules
            .iter()
            .flat_map(|module| module.index.files())
            .map(str::to_string)
            .collect();
        files.sort();
        files.dedup();
        files
    }

    fn lines(&self, file: &str) -> Vec<u32> {
        let state = self.shared.state();
        let mut lines: Vec<u32> = state
            .modules
            .iter()
            .flat_map(|module| module.index.lines(file))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    fn stopped(&self) -> Option<LineStop> {
        self.shared.state().stopped.clone()
    }

    fn next_stop(&self) -> impl Future<Output = LineStop> + MaybeSend {
        let shared = self.shared.clone();
        futures::future::poll_fn(move |context| {
            let mut state = shared.state();
            if let Some(stop) = state.unseen.pop_front() {
                return Poll::Ready(stop);
            }
            state.wakers.push(context.waker().clone());
            Poll::Pending
        })
    }

    fn set_breakpoint(&self, file: &str, line: u32) -> Option<u32> {
        let actual = self
            .lines(file)
            .into_iter()
            .find(|candidate| *candidate >= line)?;
        let mut state = self.shared.state();
        state
            .asked
            .entry(file.to_string())
            .or_default()
            .insert(line, actual);
        drop(state);
        self.shared.notify_store();
        Some(actual)
    }

    fn clear_breakpoint(&self, file: &str, line: u32) {
        let mut state = self.shared.state();
        if let Some(asked) = state.asked.get_mut(file) {
            asked.remove(&line);
        }
        drop(state);
        self.shared.notify_store();
    }

    fn pause(&self) {
        let mut state = self.shared.state();
        if state.stopped.is_some() {
            return;
        }
        state.pause = true;
        drop(state);
        self.shared.notify_store();
    }

    fn cancel_pause(&self) {
        let mut state = self.shared.state();
        if state.stopped.is_some() {
            return;
        }
        state.pause = false;
        drop(state);
        self.shared.notify_store();
    }

    fn resume(&self) {
        Shared::go_on(&mut self.shared.state());
    }

    fn step_in(&self) {
        self.shared.step(StepKind::Into);
    }

    fn step_over(&self) {
        self.shared.step(StepKind::Over);
    }

    fn step_out(&self) {
        self.shared.step(StepKind::Out);
    }

    fn release(&self) {
        let mut state = self.shared.state();
        state.asked.clear();
        state.pause = false;
        state.step = None;
        Shared::go_on(&mut state);
        drop(state);
        self.shared.notify_store();
    }
}
