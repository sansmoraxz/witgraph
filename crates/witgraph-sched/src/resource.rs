//! Per-resource weighted semaphore, by the unit that holds claims: a node, or
//! a stream group.
//!
//! A named resource has a capacity of 1.0 unless the graph declares otherwise. A node's claim is held
//! while it runs. A group's is the sum of its members' claims, held from when it
//! starts until it has drained. Everything is released when the graph stops.

use std::collections::{BTreeMap, HashMap, HashSet};

use witgraph_ir::ResourceId;

/// Tolerance for summing claims.
const EPSILON: f64 = 1e-9;

/// Tracks the allocated amount of each named resource.
#[derive(Debug, Default)]
pub(crate) struct ResourcePool {
    claims: HashMap<usize, BTreeMap<ResourceId, f64>>,
    allocated: HashMap<ResourceId, f64>,
    held_by: HashSet<usize>,
    /// What the graph declares: a resource not here has 1.0.
    capacities: HashMap<ResourceId, f64>,
}

impl ResourcePool {
    /// A pool with these capacities.
    pub(crate) fn with_capacities(capacities: HashMap<ResourceId, f64>) -> Self {
        Self {
            capacities,
            ..Self::default()
        }
    }

    fn capacity(&self, resource: &ResourceId) -> f64 {
        self.capacities.get(resource).copied().unwrap_or(1.0)
    }

    /// Registers an unit's summed claims. Returns the first resource the
    /// unit over-commits on its own (more than there is), which it could
    /// never acquire.
    pub(crate) fn register(
        &mut self,
        unit: usize,
        claims: BTreeMap<ResourceId, f64>,
    ) -> Result<(), ResourceId> {
        let over = claims
            .iter()
            .find(|(resource, claimed)| **claimed > self.capacity(resource) + EPSILON);
        if let Some((resource, _)) = over {
            return Err(resource.clone());
        }
        if !claims.is_empty() {
            self.claims.insert(unit, claims);
        }
        Ok(())
    }

    pub(crate) fn can_acquire(&self, unit: usize) -> bool {
        if self.held_by.contains(&unit) {
            return true;
        }
        let Some(claims) = self.claims.get(&unit) else {
            return true;
        };
        claims.iter().all(|(resource, amount)| {
            self.allocated.get(resource).copied().unwrap_or(0.0) + amount
                <= self.capacity(resource) + EPSILON
        })
    }

    pub(crate) fn acquire(&mut self, unit: usize) {
        if !self.held_by.insert(unit) {
            return;
        }
        if let Some(claims) = self.claims.get(&unit) {
            for (resource, amount) in claims {
                *self.allocated.entry(resource.clone()).or_insert(0.0) += amount;
            }
        }
    }

    /// Releases everything the unit holds. Idempotent.
    pub(crate) fn release(&mut self, unit: usize) {
        if !self.held_by.remove(&unit) {
            return;
        }
        let Some(claims) = self.claims.get(&unit) else {
            return;
        };
        for (resource, amount) in claims {
            if let Some(alloc) = self.allocated.get_mut(resource) {
                *alloc -= amount;
                if *alloc < EPSILON {
                    self.allocated.remove(resource);
                }
            }
        }
    }

    #[cfg(test)]
    fn is_held(&self, unit: usize) -> bool {
        self.held_by.contains(&unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with(entries: &[(usize, &[(&str, f64)])]) -> ResourcePool {
        let mut pool = ResourcePool::default();
        for (unit, claims) in entries {
            let claims = claims
                .iter()
                .map(|(r, f)| (ResourceId::from((*r).to_string()), *f))
                .collect();
            pool.register(*unit, claims).unwrap();
        }
        pool
    }

    #[test]
    fn a_capacity_is_how_much_can_be_held_at_once() {
        let gpu: ResourceId = "gpu".into();
        let mut pool = ResourcePool::with_capacities(HashMap::from([(gpu.clone(), 2.0)]));
        for unit in 0..3 {
            pool.register(unit, BTreeMap::from([(gpu.clone(), 1.0)]))
                .unwrap();
        }
        assert!(pool.can_acquire(0));
        pool.acquire(0);
        assert!(pool.can_acquire(1), "there are two");
        pool.acquire(1);
        assert!(!pool.can_acquire(2), "and both are held");
        pool.release(0);
        assert!(pool.can_acquire(2));

        let mut small = ResourcePool::with_capacities(HashMap::from([(gpu.clone(), 0.5)]));
        assert_eq!(
            small.register(0, BTreeMap::from([(gpu.clone(), 0.8)])),
            Err(gpu)
        );
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
    fn over_committed_unit_is_rejected() {
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
