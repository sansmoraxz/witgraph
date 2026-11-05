//! Holding a node's runs before they start: the graph level of a debugger, beside the lines
//! of [`crate::lines`].

use std::collections::BTreeSet;
use std::future::Future;

use witgraph_ir::NodeId;

use crate::executor::MaybeSend;

/// Whose runs wait before they start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldAt {
    /// These nodes'. None, by default: nothing is held.
    Nodes(BTreeSet<NodeId>),
    /// Every node's, as stepping through the graph a node at a time does.
    All,
}

impl Default for HoldAt {
    /// Nothing held.
    fn default() -> Self {
        HoldAt::Nodes(BTreeSet::new())
    }
}

impl HoldAt {
    /// Whether `node`'s runs wait.
    #[must_use]
    pub fn holds(&self, node: &NodeId) -> bool {
        match self {
            HoldAt::Nodes(nodes) => nodes.contains(node),
            HoldAt::All => true,
        }
    }
}

/// Which held run: what [`NodeHolds::resume`] lets go of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HoldId(pub u64);

/// A run held before it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRun {
    /// What lets it go.
    pub id: HoldId,
    /// Whose run it is.
    pub node: NodeId,
}

/// Holds node runs before they start, and lets them go. The scheduler takes a held run to have
/// started, so what reads its outputs waits for it, and the tick goes on with the rest.
///
/// A host calls this from outside the tick, which a held run keeps from ending. A run held when
/// the graph is shut down or rebuilt is dropped, as the run would have been.
pub trait NodeHolds {
    /// Holds the runs of these nodes from the next to start. What is held already stays.
    fn hold_at(&self, at: HoldAt);

    /// The runs held now, in the order they were held.
    fn held(&self) -> Vec<HeldRun>;

    /// The next run held that has not been told of.
    fn next_held(&self) -> impl Future<Output = HeldRun> + MaybeSend;

    /// Lets the run go. Nothing, if it is not held.
    fn resume(&self, id: HoldId);

    /// Holds nothing from now on, and lets go of all that is held.
    fn release(&self);
}
