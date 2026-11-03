//! Dense indices into a compiled graph's ports and connections.
//!
//! The scheduler and the code it drives agree on ports by number, so a
//! number used as the wrong kind of index is a wrong value with no error.
//! Each kind of index is its own type here, and an [`IndexVec`] or
//! [`PortSet`] only takes the kind it was made for. Every index is a `u32`
//! with no cost over one. Names ([`NodeId`], [`PortName`]) resolve to
//! indices once, at the API, through a [`PortTable`].

use core::fmt;
use core::marker::PhantomData;
use core::ops::{Index, IndexMut};
use std::collections::HashMap;

use crate::compile::CompiledGraph;
use crate::id::{ConnectionId, NodeId, PortName, PortRef};
use crate::port::PortKind;

/// A dense index: a position in an [`IndexVec`] or [`PortSet`].
pub trait Idx: Copy + Eq + fmt::Debug + 'static {
    /// The index at `position`.
    fn at(position: usize) -> Self;

    /// Its position.
    fn position(self) -> usize;
}

macro_rules! index_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name(u32);

        impl Idx for $name {
            #[inline]
            fn at(position: usize) -> Self {
                debug_assert!(u32::try_from(position).is_ok());
                Self(position as u32)
            }

            #[inline]
            fn position(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl $name {
            /// The index at `position`.
            pub fn from_position(position: usize) -> Self {
                <Self as Idx>::at(position)
            }
        }

        const _: () = assert!(size_of::<$name>() == size_of::<u32>());
    };
}

index_type! {
    /// A node's position in a topological order of the graph.
    NodeIdx
}

index_type! {
    /// An input port of any node, of any kind.
    InputIdx
}

index_type! {
    /// An output port of any node, of any kind.
    OutputIdx
}

index_type! {
    /// A feedback connection, numbered in the order of their ids.
    FeedbackIdx
}

/// A slice that only its own kind of index can index.
#[derive(Clone, PartialEq, Eq)]
pub struct IndexVec<I, T> {
    items: Box<[T]>,
    kind: PhantomData<fn(I) -> I>,
}

impl<I: Idx, T> IndexVec<I, T> {
    /// `len` items, each made by `item` from its index.
    pub fn from_fn(len: usize, mut item: impl FnMut(I) -> T) -> Self {
        (0..len).map(|i| item(I::at(i))).collect()
    }

    /// How many items there are.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The item at `index`, if there is one.
    pub fn get(&self, index: I) -> Option<&T> {
        self.items.get(index.position())
    }

    /// Every item, in index order.
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.items.iter()
    }

    /// Every item with its index, in index order.
    pub fn iter_enumerated(&self) -> impl DoubleEndedIterator<Item = (I, &T)> {
        self.items.iter().enumerate().map(|(i, t)| (I::at(i), t))
    }

    /// Every item with its index, in index order, to change.
    pub fn iter_enumerated_mut(&mut self) -> impl DoubleEndedIterator<Item = (I, &mut T)> {
        self.items
            .iter_mut()
            .enumerate()
            .map(|(i, t)| (I::at(i), t))
    }
}

impl<I: Idx, T> FromIterator<T> for IndexVec<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        Self {
            items: iter.into_iter().collect(),
            kind: PhantomData,
        }
    }
}

impl<I: Idx, T> Index<I> for IndexVec<I, T> {
    type Output = T;

    #[inline]
    fn index(&self, index: I) -> &T {
        &self.items[index.position()]
    }
}

impl<I: Idx, T> IndexMut<I> for IndexVec<I, T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut T {
        &mut self.items[index.position()]
    }
}

impl<I, T: fmt::Debug> fmt::Debug for IndexVec<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.items.fmt(f)
    }
}

/// A set of indices of one kind, as bits.
#[derive(Clone, PartialEq, Eq)]
pub struct PortSet<I> {
    words: Box<[u64]>,
    kind: PhantomData<fn(I) -> I>,
}

impl<I: Idx> PortSet<I> {
    /// An empty set of indices below `len`.
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)].into_boxed_slice(),
            kind: PhantomData,
        }
    }

    /// Whether `index` is in the set.
    #[inline]
    pub fn contains(&self, index: I) -> bool {
        let position = index.position();
        self.words[position / 64] & (1 << (position % 64)) != 0
    }

    /// Adds `index`. Returns whether it was not there before.
    #[inline]
    pub fn insert(&mut self, index: I) -> bool {
        let position = index.position();
        let (word, bit) = (&mut self.words[position / 64], 1 << (position % 64));
        let added = *word & bit == 0;
        *word |= bit;
        added
    }

    /// Removes `index`. Returns whether it was there.
    #[inline]
    pub fn remove(&mut self, index: I) -> bool {
        let position = index.position();
        let (word, bit) = (&mut self.words[position / 64], 1 << (position % 64));
        let removed = *word & bit != 0;
        *word &= !bit;
        removed
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Empties the set.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// Every index in the set, in order.
    pub fn iter(&self) -> impl Iterator<Item = I> + '_ {
        self.words.iter().enumerate().flat_map(|(at, word)| {
            let mut bits = *word;
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(I::at(at * 64 + bit))
            })
        })
    }
}

impl<I: Idx> fmt::Debug for PortSet<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// One numbered port.
#[derive(Debug, Clone)]
pub struct PortEntry {
    /// The port.
    pub port: PortRef,
    /// Its node.
    pub node: NodeIdx,
    /// Its kind.
    pub kind: PortKind,
}

/// One feedback connection, numbered.
#[derive(Debug, Clone)]
pub struct FeedbackEntry {
    /// The connection.
    pub id: ConnectionId,
    /// The output it reads.
    pub from: OutputIdx,
    /// The input it writes.
    pub to: InputIdx,
    /// Whether it delivers an option's payload
    /// ([`PortDef::unwraps_into`](crate::PortDef::unwraps_into)).
    pub unwraps_option: bool,
}

/// One node's ports, by name. Nodes have few ports, so a scan is both
/// faster than hashing and asks for no owned key.
#[derive(Debug, Clone, Default)]
struct NodePorts {
    inputs: Vec<(PortName, InputIdx)>,
    outputs: Vec<(PortName, OutputIdx)>,
}

/// Every port and feedback connection of a compiled graph, numbered, and
/// how names resolve to those numbers. A pure function of the graph, so
/// whatever numbers a graph's ports from one agrees on them.
#[derive(Debug, Clone)]
pub struct PortTable {
    nodes: IndexVec<NodeIdx, NodeId>,
    index: HashMap<NodeId, NodeIdx>,
    ports: IndexVec<NodeIdx, NodePorts>,
    inputs: IndexVec<InputIdx, PortEntry>,
    outputs: IndexVec<OutputIdx, PortEntry>,
    feedback: IndexVec<FeedbackIdx, FeedbackEntry>,
    feedback_index: HashMap<ConnectionId, FeedbackIdx>,
}

impl PortTable {
    /// Numbers `compiled`'s nodes in the order of its islands and their
    /// members, which is topological, each node's ports in the order its
    /// contract declares them, and its feedback connections by id.
    pub fn new(compiled: &CompiledGraph) -> Self {
        let order: Vec<&NodeId> = compiled.islands().iter().flatten().collect();
        let nodes: IndexVec<NodeIdx, NodeId> = order.iter().map(|n| (*n).clone()).collect();
        let index: HashMap<NodeId, NodeIdx> = nodes
            .iter_enumerated()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        let mut ports = Vec::with_capacity(nodes.len());
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for (at, node) in nodes.iter_enumerated() {
            let mut own = NodePorts::default();
            if let Some(contract) = compiled.contract_for(node) {
                for port in &contract.inputs {
                    own.inputs
                        .push((port.name.clone(), InputIdx::at(inputs.len())));
                    inputs.push(PortEntry {
                        port: PortRef::new(node.clone(), port.name.clone()),
                        node: at,
                        kind: port.kind,
                    });
                }
                for port in &contract.outputs {
                    own.outputs
                        .push((port.name.clone(), OutputIdx::at(outputs.len())));
                    outputs.push(PortEntry {
                        port: PortRef::new(node.clone(), port.name.clone()),
                        node: at,
                        kind: port.kind,
                    });
                }
            }
            ports.push(own);
        }
        let mut table = Self {
            nodes,
            index,
            ports: ports.into_iter().collect(),
            inputs: inputs.into_iter().collect(),
            outputs: outputs.into_iter().collect(),
            feedback: IndexVec::from_iter([]),
            feedback_index: HashMap::new(),
        };
        let mut feedback: Vec<FeedbackEntry> = compiled
            .connections()
            .filter(|resolved| resolved.connection.feedback)
            .filter_map(|resolved| {
                let conn = resolved.connection;
                Some(FeedbackEntry {
                    id: conn.id.clone(),
                    from: table.output(&conn.from.node, &conn.from.port)?,
                    to: table.input(&conn.to.node, &conn.to.port)?,
                    unwraps_option: resolved.unwraps_option,
                })
            })
            .collect();
        feedback.sort_by(|a, b| a.id.cmp(&b.id));
        table.feedback = feedback.into_iter().collect();
        table.feedback_index = table
            .feedback
            .iter_enumerated()
            .map(|(i, entry)| (entry.id.clone(), i))
            .collect();
        table
    }

    /// How many nodes there are.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The node at `index`.
    pub fn node_id(&self, index: NodeIdx) -> &NodeId {
        &self.nodes[index]
    }

    /// The index of `node`, if the graph has it.
    pub fn node(&self, node: &NodeId) -> Option<NodeIdx> {
        self.index.get(node).copied()
    }

    /// The index of `node`'s input `port`, of any kind.
    pub fn input(&self, node: &NodeId, port: &PortName) -> Option<InputIdx> {
        let ports = &self.ports[self.node(node)?].inputs;
        ports.iter().find(|(p, _)| p == port).map(|(_, i)| *i)
    }

    /// The index of `node`'s output `port`, of any kind.
    pub fn output(&self, node: &NodeId, port: &PortName) -> Option<OutputIdx> {
        let ports = &self.ports[self.node(node)?].outputs;
        ports.iter().find(|(p, _)| p == port).map(|(_, i)| *i)
    }

    /// Every input port.
    pub fn inputs(&self) -> &IndexVec<InputIdx, PortEntry> {
        &self.inputs
    }

    /// Every output port.
    pub fn outputs(&self) -> &IndexVec<OutputIdx, PortEntry> {
        &self.outputs
    }

    /// Every feedback connection.
    pub fn feedback(&self) -> &IndexVec<FeedbackIdx, FeedbackEntry> {
        &self.feedback
    }

    /// The index of feedback connection `id`, if it is one.
    pub fn feedback_index(&self, id: &ConnectionId) -> Option<FeedbackIdx> {
        self.feedback_index.get(id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_holds_what_was_inserted_across_words() {
        let mut set = PortSet::<InputIdx>::new(130);
        assert!(set.is_empty());
        for at in [0, 63, 64, 129] {
            assert!(set.insert(InputIdx::at(at)));
            assert!(!set.insert(InputIdx::at(at)), "already there");
        }
        assert!(set.contains(InputIdx::at(64)));
        assert!(!set.contains(InputIdx::at(65)));
        let all: Vec<usize> = set.iter().map(InputIdx::position).collect();
        assert_eq!(all, [0, 63, 64, 129]);
        assert!(set.remove(InputIdx::at(64)));
        assert!(!set.remove(InputIdx::at(64)));
        set.clear();
        assert!(set.is_empty());
    }

    #[test]
    fn a_set_of_nothing_allocates_no_words() {
        let set = PortSet::<FeedbackIdx>::new(0);
        assert!(set.is_empty());
        assert_eq!(set.iter().count(), 0);
    }

    #[test]
    fn an_index_vec_makes_items_from_their_indices() {
        let mut v = IndexVec::<OutputIdx, usize>::from_fn(3, OutputIdx::position);
        assert_eq!(v[OutputIdx::at(2)], 2);
        v[OutputIdx::at(2)] = 9;
        assert_eq!(v.get(OutputIdx::at(2)), Some(&9));
        assert_eq!(v.get(OutputIdx::at(3)), None);
        let (last, item) = v.iter_enumerated().next_back().unwrap();
        assert_eq!((last, *item), (OutputIdx::at(2), 9));
    }
}
