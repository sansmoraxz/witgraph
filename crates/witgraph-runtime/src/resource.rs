//! Per-resource weighted semaphore, at island granularity.
//!
//! Each named resource has an implicit capacity of 1.0. An island's claim
//! on a resource is the sum of its nodes' claims; it is acquired when the
//! island starts a generation and released when the generation finishes,
//! faults or is cancelled.

use std::collections::{BTreeMap, HashMap, HashSet};

use witgraph_ir::ResourceId;

/// Tolerance for summing fractional claims.
const EPSILON: f64 = 1e-9;

/// Tracks the allocated fraction of each named resource.
#[derive(Debug, Default)]
pub(crate) struct ResourcePool {
    claims: HashMap<usize, BTreeMap<ResourceId, f64>>,
    allocated: HashMap<ResourceId, f64>,
    held_by: HashSet<usize>,
}

impl ResourcePool {
    /// Registers an island's summed claims. Returns the first resource the
    /// island over-commits on its own (more than 1.0), which it could
    /// never acquire.
    pub(crate) fn register(
        &mut self,
        island: usize,
        claims: BTreeMap<ResourceId, f64>,
    ) -> Result<(), ResourceId> {
        if let Some((resource, _)) = claims.iter().find(|(_, f)| **f > 1.0 + EPSILON) {
            return Err(resource.clone());
        }
        if !claims.is_empty() {
            self.claims.insert(island, claims);
        }
        Ok(())
    }

    pub(crate) fn can_acquire(&self, island: usize) -> bool {
        if self.held_by.contains(&island) {
            return true;
        }
        let Some(claims) = self.claims.get(&island) else {
            return true;
        };
        claims.iter().all(|(resource, fraction)| {
            self.allocated.get(resource).copied().unwrap_or(0.0) + fraction <= 1.0 + EPSILON
        })
    }

    pub(crate) fn acquire(&mut self, island: usize) {
        if !self.held_by.insert(island) {
            return;
        }
        if let Some(claims) = self.claims.get(&island) {
            for (resource, fraction) in claims {
                *self.allocated.entry(resource.clone()).or_insert(0.0) += fraction;
            }
        }
    }

    /// Releases everything the island holds. Idempotent.
    pub(crate) fn release(&mut self, island: usize) {
        if !self.held_by.remove(&island) {
            return;
        }
        let Some(claims) = self.claims.get(&island) else {
            return;
        };
        for (resource, fraction) in claims {
            if let Some(alloc) = self.allocated.get_mut(resource) {
                *alloc -= fraction;
                if *alloc < EPSILON {
                    self.allocated.remove(resource);
                }
            }
        }
    }

    #[cfg(test)]
    fn is_held(&self, island: usize) -> bool {
        self.held_by.contains(&island)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with(entries: &[(usize, &[(&str, f64)])]) -> ResourcePool {
        let mut pool = ResourcePool::default();
        for (island, claims) in entries {
            let claims = claims
                .iter()
                .map(|(r, f)| (ResourceId::from((*r).to_string()), *f))
                .collect();
            pool.register(*island, claims).unwrap();
        }
        pool
    }

    #[test]
    fn acquire_and_release() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)])]);
        assert!(pool.can_acquire(0));
        pool.acquire(0);
        assert!(pool.is_held(0));
        pool.release(0);
        assert!(!pool.is_held(0));
    }

    #[test]
    fn mutual_exclusion() {
        let mut pool = pool_with(&[(0, &[("disk", 0.6)]), (1, &[("disk", 0.6)])]);
        pool.acquire(0);
        assert!(!pool.can_acquire(1));
        pool.release(0);
        assert!(pool.can_acquire(1));
    }

    #[test]
    fn compatible_claims() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)]), (1, &[("disk", 0.5)])]);
        pool.acquire(0);
        assert!(pool.can_acquire(1));
        pool.acquire(1);
        assert!(pool.is_held(0) && pool.is_held(1));
    }

    #[test]
    fn no_claims_always_acquires() {
        assert!(ResourcePool::default().can_acquire(7));
    }

    #[test]
    fn release_and_acquire_are_idempotent() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)]), (1, &[("disk", 0.5)])]);
        pool.acquire(0);
        pool.acquire(0);
        assert!(
            pool.can_acquire(1),
            "a repeated acquire must not double-count"
        );
        pool.release(0);
        pool.release(0);
        assert!(!pool.is_held(0));
    }

    #[test]
    fn multiple_resources_checked_together() {
        let mut pool = pool_with(&[
            (0, &[("disk", 0.6), ("gpu", 0.3)]),
            (1, &[("disk", 0.3), ("gpu", 0.8)]),
        ]);
        pool.acquire(0);
        // disk: 0.6 + 0.3 = 0.9 OK; gpu: 0.3 + 0.8 = 1.1 > 1.0
        assert!(!pool.can_acquire(1));
    }

    #[test]
    fn over_committed_island_is_rejected() {
        let mut pool = ResourcePool::default();
        let claims = [(ResourceId::from("disk".to_string()), 1.2)]
            .into_iter()
            .collect();
        assert_eq!(
            pool.register(0, claims),
            Err(ResourceId::from("disk".to_string()))
        );
    }
}
