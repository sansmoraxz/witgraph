//! What streams between nodes carry, for previews: [`StreamPreviews`].
//!
//! A stream passes from one node's output to another's input inside the composed graph; the
//! host never holds it. To see its items, the plane learns which stream each output's is, as
//! the hub holds it between the writer's run returning and the reader's starting (it takes it
//! from the hub and gives it back unread), and wasmtime tells a [`StreamObserver`] of each
//! item it copies from writer to reader. Nothing is read twice and nothing is held up: the
//! reader gets every item as it would have.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use wasmtime::component::{StreamId, StreamObserver, Val};
use witgraph_ir::{CompiledGraph, InputIdx, NodeId, NodeIdx, PortName, PortTable};

/// How many items of each output are kept, the latest.
const KEPT: usize = 64;

/// The items one stream output carried since they were last taken: its latest (as WAVE text,
/// oldest first) and how many in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamItems {
    /// The node writing them.
    pub node: NodeId,
    /// Its output.
    pub port: PortName,
    /// The latest items, oldest first, each as WAVE text: at most 64.
    pub items: Vec<String>,
    /// How many items it carried since they were last taken, those not kept too.
    pub count: u64,
}

#[derive(Default)]
struct State {
    /// The streams watched, and the output each is.
    watched: HashMap<StreamId, (NodeId, PortName)>,
    /// By output, what came since it was last taken.
    seen: BTreeMap<(NodeId, PortName), (VecDeque<String>, u64)>,
}

/// What streams between a graph's nodes carry: [`RuntimeGraph::stream_previews`]
/// (crate::RuntimeGraph::stream_previews), when loaded with
/// [`RuntimeConfig::stream_previews`](crate::RuntimeConfig::stream_previews). Clone it to hold
/// on to it while a tick runs.
#[derive(Clone)]
pub struct StreamPreviews {
    state: Arc<Mutex<State>>,
    /// By writing node, each stream output it has and the input reading it.
    edges: Arc<HashMap<NodeIdx, Vec<(PortName, InputIdx)>>>,
}

impl StreamPreviews {
    /// The previews of `compiled`'s streams, numbered as `ports` has it.
    pub(crate) fn new(compiled: &CompiledGraph, ports: &PortTable) -> Self {
        let mut edges: HashMap<NodeIdx, Vec<(PortName, InputIdx)>> = HashMap::new();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            if !matches!(resolved.from.kind(), witgraph_ir::PortKind::Stream) {
                continue;
            }
            if let (Some(node), Some(input)) = (
                ports.node(&conn.from.node),
                ports.input(&conn.to.node, &conn.to.port),
            ) {
                edges
                    .entry(node)
                    .or_default()
                    .push((conn.from.port.clone(), input));
            }
        }
        Self {
            state: Arc::new(Mutex::new(State::default())),
            edges: Arc::new(edges),
        }
    }

    /// What every stream output carried since this was last asked, output by output.
    pub fn take(&self) -> Vec<StreamItems> {
        let mut state = lock(&self.state);
        std::mem::take(&mut state.seen)
            .into_iter()
            .filter(|(_, (_, count))| *count > 0)
            .map(|((node, port), (items, count))| StreamItems {
                node,
                port,
                items: items.into(),
                count,
            })
            .collect()
    }

    /// The stream outputs of `node`, and the input reading each.
    pub(crate) fn edges_of(&self, node: NodeIdx) -> &[(PortName, InputIdx)] {
        self.edges.get(&node).map_or(&[], Vec::as_slice)
    }

    /// `stream` is `node`'s output `port`'s, from now on.
    pub(crate) fn watch(&self, stream: StreamId, node: NodeId, port: PortName) {
        lock(&self.state).watched.insert(stream, (node, port));
    }

    /// What tells this of the items wasmtime copies.
    pub(crate) fn observer(&self) -> impl StreamObserver {
        Observer(self.state.clone())
    }
}

struct Observer(Arc<Mutex<State>>);

impl StreamObserver for Observer {
    fn observes(&mut self, stream: StreamId) -> bool {
        lock(&self.0).watched.contains_key(&stream)
    }

    fn items(&mut self, stream: StreamId, items: &[Val]) {
        let mut state = lock(&self.0);
        let Some(at) = state.watched.get(&stream).cloned() else {
            return;
        };
        let (kept, count) = state.seen.entry(at).or_default();
        *count += items.len() as u64;
        for item in items.iter().rev().take(KEPT).rev() {
            let text = item.to_wave().unwrap_or_else(|e| format!("<{e}>"));
            if kept.len() == KEPT {
                kept.pop_front();
            }
            kept.push_back(text);
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
