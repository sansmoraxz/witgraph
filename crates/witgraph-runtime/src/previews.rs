//! What streams between nodes carry, for previews: [`StreamPreviews`].
//!
//! A stream passes from one node's output to another's input inside the composed graph; the
//! host never holds it. Composed with stream taps, the hub moves each stream's items on itself
//! (`witgraph_wit::hub::tap_stream`), and shows the host each chunk through its
//! `witgraph:hub/preview` import, which this implements. The reader gets every item as it
//! would have, and a write still waits for it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use wasmtime::component::Val;
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
    /// By writing node, the stream inputs its outputs feed.
    edges: Arc<HashMap<NodeIdx, Vec<InputIdx>>>,
    /// By stream input, the output feeding it.
    writers: Arc<HashMap<InputIdx, (NodeId, PortName)>>,
}

impl StreamPreviews {
    /// The previews of `compiled`'s streams, numbered as `ports` has it.
    pub(crate) fn new(compiled: &CompiledGraph, ports: &PortTable) -> Self {
        let mut edges: HashMap<NodeIdx, Vec<InputIdx>> = HashMap::new();
        let mut writers = HashMap::new();
        for resolved in compiled.connections() {
            let conn = resolved.connection;
            if !matches!(resolved.from.kind(), witgraph_ir::PortKind::Stream) {
                continue;
            }
            if let (Some(node), Some(input)) = (
                ports.node(&conn.from.node),
                ports.input(&conn.to.node, &conn.to.port),
            ) {
                edges.entry(node).or_default().push(input);
                writers.insert(input, (conn.from.node.clone(), conn.from.port.clone()));
            }
        }
        Self {
            state: Arc::new(Mutex::new(State::default())),
            edges: Arc::new(edges),
            writers: Arc::new(writers),
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

    /// The stream inputs `node`'s outputs feed.
    pub(crate) fn inputs_fed_by(&self, node: NodeIdx) -> &[InputIdx] {
        self.edges.get(&node).map_or(&[], Vec::as_slice)
    }

    /// The stream inputs, and the output feeding each.
    pub(crate) fn writers(&self) -> impl Iterator<Item = (InputIdx, &(NodeId, PortName))> {
        self.writers.iter().map(|(input, at)| (*input, at))
    }

    /// `items` passed on to `input`: of the output feeding it, which keeps the latest.
    pub(crate) fn record(&self, input: InputIdx, items: &[Val]) {
        let Some(at) = self.writers.get(&input) else {
            return;
        };
        let mut state = lock(&self.state);
        let (kept, count) = state.seen.entry(at.clone()).or_default();
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
