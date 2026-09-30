//! Per-resource weighted semaphore for concurrency control.
//!
//! Each named resource has an implicit capacity of 1.0. Active nodes hold
//! fractional claims; the scheduler checks availability before activation
//! and releases claims when nodes complete, fault, or cancel.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use witgraph_ir::{NodeId, ResourceClaim, ResourceId};

/// Tracks the allocated fraction of each named resource.
#[derive(Debug, Default)]
pub(crate) struct ResourcePool {
    node_claims: HashMap<NodeId, BTreeMap<ResourceId, ResourceClaim>>,
    allocated: HashMap<ResourceId, f64>,
    held_by: HashSet<NodeId>,
}

impl ResourcePool {
    pub(crate) fn from_nodes(nodes: &[(NodeId, BTreeMap<ResourceId, ResourceClaim>)]) -> Self {
        let mut pool = Self::default();
        for (node_id, claims) in nodes {
            if !claims.is_empty() {
                pool.node_claims.insert(node_id.clone(), claims.clone());
            }
        }
        pool
    }

    pub(crate) fn can_acquire(&self, node: &NodeId) -> bool {
        let Some(claims) = self.node_claims.get(node) else {
            return true;
        };
        for (resource, claim) in claims {
            let current = self.allocated.get(resource).copied().unwrap_or(0.0);
            if current + claim.fraction.get() > 1.0 + f64::EPSILON {
                return false;
            }
        }
        true
    }

    pub(crate) fn acquire(&mut self, node: &NodeId) {
        let Some(claims) = self.node_claims.get(node) else {
            return;
        };
        for (resource, claim) in claims {
            *self.allocated.entry(resource.clone()).or_insert(0.0) += claim.fraction.get();
        }
        self.held_by.insert(node.clone());
    }

    /// Releases ALL claims for a node. Used on Complete, Fault, Cancel.
    pub(crate) fn release(&mut self, node: &NodeId) {
        if !self.held_by.remove(node) {
            return;
        }
        self.subtract_claims(node, |_| true);
    }

    /// Releases only non-held claims (`hold == false`). Used on Suspend.
    /// Returns `true` if the node still holds any resources after release.
    pub(crate) fn release_non_held(&mut self, node: &NodeId) -> bool {
        if !self.held_by.contains(node) {
            return false;
        }
        self.subtract_claims(node, |claim| !claim.hold);
        let still_holds = self
            .node_claims
            .get(node)
            .is_some_and(|claims| claims.values().any(|c| c.hold));
        if !still_holds {
            self.held_by.remove(node);
        }
        still_holds
    }

    pub(crate) fn is_held(&self, node: &NodeId) -> bool {
        self.held_by.contains(node)
    }

    /// Moves all entries from `deferred` into `ready`.
    pub(crate) fn drain_deferred_into(
        deferred: &mut BTreeSet<(usize, NodeId)>,
        ready: &mut BTreeSet<(usize, NodeId)>,
    ) {
        ready.append(deferred);
    }

    fn subtract_claims(&mut self, node: &NodeId, predicate: impl Fn(&ResourceClaim) -> bool) {
        let Some(claims) = self.node_claims.get(node) else {
            return;
        };
        for (resource, claim) in claims {
            if predicate(claim) {
                if let Some(alloc) = self.allocated.get_mut(resource) {
                    *alloc = (*alloc - claim.fraction.get()).max(0.0);
                    if *alloc < f64::EPSILON {
                        self.allocated.remove(resource);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use witgraph_ir::Fraction;

    fn pool_with(entries: &[(&str, &[(&str, f64, bool)])]) -> ResourcePool {
        let nodes: Vec<(NodeId, BTreeMap<ResourceId, ResourceClaim>)> = entries
            .iter()
            .map(|(node, claims)| {
                let claims: BTreeMap<ResourceId, ResourceClaim> = claims
                    .iter()
                    .map(|(r, f, h)| {
                        (
                            ResourceId::from(r.to_string()),
                            ResourceClaim {
                                fraction: Fraction::new(*f).unwrap(),
                                hold: *h,
                            },
                        )
                    })
                    .collect();
                (NodeId::from(node.to_string()), claims)
            })
            .collect();
        ResourcePool::from_nodes(&nodes)
    }

    #[test]
    fn acquire_and_release() {
        let mut pool = pool_with(&[("a", &[("disk", 0.5, false)])]);
        let a: NodeId = "a".into();

        assert!(pool.can_acquire(&a));
        pool.acquire(&a);
        assert!(pool.is_held(&a));

        pool.release(&a);
        assert!(!pool.is_held(&a));
    }

    #[test]
    fn mutual_exclusion() {
        let mut pool = pool_with(&[
            ("a", &[("disk", 0.6, false)]),
            ("b", &[("disk", 0.6, false)]),
        ]);
        let a: NodeId = "a".into();
        let b: NodeId = "b".into();

        pool.acquire(&a);
        assert!(!pool.can_acquire(&b));

        pool.release(&a);
        assert!(pool.can_acquire(&b));
    }

    #[test]
    fn compatible_claims() {
        let mut pool = pool_with(&[
            ("a", &[("disk", 0.5, false)]),
            ("b", &[("disk", 0.5, false)]),
        ]);
        let a: NodeId = "a".into();
        let b: NodeId = "b".into();

        pool.acquire(&a);
        assert!(pool.can_acquire(&b));
        pool.acquire(&b);
        assert!(pool.is_held(&a));
        assert!(pool.is_held(&b));
    }

    #[test]
    fn release_non_held_frees_compute_keeps_vram() {
        let mut pool = pool_with(&[("a", &[("gpu_compute", 0.5, false), ("vram", 0.3, true)])]);
        let a: NodeId = "a".into();

        pool.acquire(&a);
        let still_holds = pool.release_non_held(&a);
        assert!(still_holds);
        assert!(pool.is_held(&a));

        // gpu_compute freed, vram still allocated
        assert_eq!(pool.allocated.get(&ResourceId::from("gpu_compute".to_string())), None);
        assert!(pool.allocated.get(&ResourceId::from("vram".to_string())).is_some());
    }

    #[test]
    fn release_non_held_all_non_held_clears_holder() {
        let mut pool = pool_with(&[("a", &[("disk", 0.5, false)])]);
        let a: NodeId = "a".into();

        pool.acquire(&a);
        let still_holds = pool.release_non_held(&a);
        assert!(!still_holds);
        assert!(!pool.is_held(&a));
    }

    #[test]
    fn no_claims_always_acquires() {
        let pool = ResourcePool::default();
        let x: NodeId = "x".into();
        assert!(pool.can_acquire(&x));
    }

    #[test]
    fn release_is_idempotent() {
        let mut pool = pool_with(&[("a", &[("disk", 0.5, false)])]);
        let a: NodeId = "a".into();
        pool.acquire(&a);
        pool.release(&a);
        pool.release(&a); // second release is a no-op
        assert!(!pool.is_held(&a));
    }

    #[test]
    fn multiple_resources_checked_together() {
        let mut pool = pool_with(&[
            ("a", &[("disk", 0.6, false), ("gpu", 0.3, false)]),
            ("b", &[("disk", 0.3, false), ("gpu", 0.8, false)]),
        ]);
        let a: NodeId = "a".into();
        let b: NodeId = "b".into();

        pool.acquire(&a);
        // disk: 0.6 + 0.3 = 0.9 OK; gpu: 0.3 + 0.8 = 1.1 > 1.0
        assert!(!pool.can_acquire(&b));
    }
}
