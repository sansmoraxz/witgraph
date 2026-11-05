//! Holding node runs before they start, on wasmtime: [`NodeHolds`].

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

use futures::channel::oneshot;
use witgraph_ir::NodeId;
use witgraph_traits::MaybeSend;
use witgraph_traits::holds::{HeldRun, HoldAt, HoldId, NodeHolds};

#[derive(Default)]
struct State {
    at: HoldAt,
    next: u64,
    /// What is held, and what lets each go.
    parked: BTreeMap<HoldId, (NodeId, oneshot::Sender<()>)>,
    /// Holds the host has not been told of.
    unseen: VecDeque<HeldRun>,
    wakers: Vec<Waker>,
}

/// Holds a graph's node runs before they start, and lets them go. Clone it to hold on to it
/// while a tick runs: [`RuntimeGraph::holds`](crate::RuntimeGraph::holds).
#[derive(Clone, Default)]
pub struct RuntimeHolds {
    state: Arc<Mutex<State>>,
}

/// A run held, until it is let go or dropped with what serves the graph.
pub(crate) struct Parked {
    id: HoldId,
    state: Arc<Mutex<State>>,
    pub(crate) go: oneshot::Receiver<()>,
}

impl Drop for Parked {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        state.parked.remove(&self.id);
        let id = self.id;
        state.unseen.retain(|held| held.id != id);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RuntimeHolds {
    /// Holds `node`'s run, if its runs are held.
    pub(crate) fn park(&self, node: &NodeId) -> Option<Parked> {
        let mut state = lock(&self.state);
        if !state.at.holds(node) {
            return None;
        }
        let id = HoldId(state.next);
        state.next += 1;
        let (go, wait) = oneshot::channel();
        state.parked.insert(id, (node.clone(), go));
        state.unseen.push_back(HeldRun {
            id,
            node: node.clone(),
        });
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
        Some(Parked {
            id,
            state: self.state.clone(),
            go: wait,
        })
    }
}

impl NodeHolds for RuntimeHolds {
    fn hold_at(&self, at: HoldAt) {
        lock(&self.state).at = at;
    }

    fn held(&self) -> Vec<HeldRun> {
        lock(&self.state)
            .parked
            .iter()
            .map(|(id, (node, _))| HeldRun {
                id: *id,
                node: node.clone(),
            })
            .collect()
    }

    fn next_held(&self) -> impl Future<Output = HeldRun> + MaybeSend {
        let state = self.state.clone();
        futures::future::poll_fn(move |context| {
            let mut state = lock(&state);
            if let Some(held) = state.unseen.pop_front() {
                return Poll::Ready(held);
            }
            state.wakers.push(context.waker().clone());
            Poll::Pending
        })
    }

    fn resume(&self, id: HoldId) {
        let go = lock(&self.state).parked.remove(&id);
        if let Some((_, go)) = go {
            let _ = go.send(());
        }
    }

    fn release(&self) {
        let parked = {
            let mut state = lock(&self.state);
            state.at = HoldAt::default();
            state.unseen.clear();
            std::mem::take(&mut state.parked)
        };
        for (_, (_, go)) in parked {
            let _ = go.send(());
        }
    }
}
