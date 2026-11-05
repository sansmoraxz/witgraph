//! Per-resource weighted semaphore, by the node that holds claims.
//!
//! A named resource has a capacity of 1.0 unless the graph declares otherwise. A node holds
//! its claims while its `run` is in flight, which never goes past a capacity: a `run` only
//! depends on what is above it, all of which has returned, so a node holding a claim this way
//! never waits on one that is waiting for room.
//!
//! A member of a stream group goes on holding its claims once its `run` has returned, until
//! the group has drained, as its guest work goes on too. Those claims [linger](ResourcePool::linger):
//! they keep others out, but not what the group is waiting on ([`Room::PastLingering`]), since
//! streams are not buffered, so the group cannot drain without it. Everything is released when
//! the graph stops.

use std::collections::{BTreeMap, HashMap};

use witgraph_ir::ResourceId;

/// Tolerance for summing claims.
const EPSILON: f64 = 1e-9;

/// How a node holds its claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    Running,
    Lingering,
}

/// What a node may start in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Room {
    /// What nothing holds.
    Free,
    /// What no `run` in flight holds, for what a stream group at work is waiting on.
    PastLingering,
}

/// Tracks how much of each named resource is held, and how.
#[derive(Debug, Default)]
pub(crate) struct ResourcePool {
    claims: HashMap<usize, BTreeMap<ResourceId, f64>>,
    running: HashMap<ResourceId, f64>,
    lingering: HashMap<ResourceId, f64>,
    held: HashMap<usize, Hold>,
    /// What the graph declares: a resource not here has 1.0.
    capacities: HashMap<ResourceId, f64>,
}

fn add(to: &mut HashMap<ResourceId, f64>, claims: &BTreeMap<ResourceId, f64>) {
    for (resource, amount) in claims {
        *to.entry(resource.clone()).or_insert(0.0) += amount;
    }
}

fn take(from: &mut HashMap<ResourceId, f64>, claims: &BTreeMap<ResourceId, f64>) {
    for (resource, amount) in claims {
        if let Some(held) = from.get_mut(resource) {
            *held -= amount;
            if *held < EPSILON {
                from.remove(resource);
            }
        }
    }
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

    /// Registers a node's claims. Returns the first resource it claims more of than
    /// there is, which it could never acquire.
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

    pub(crate) fn can_acquire(&self, unit: usize, room: Room) -> bool {
        if self.held.contains_key(&unit) {
            return true;
        }
        let Some(claims) = self.claims.get(&unit) else {
            return true;
        };
        claims.iter().all(|(resource, amount)| {
            let mut held = self.running.get(resource).copied().unwrap_or(0.0);
            if room == Room::Free {
                held += self.lingering.get(resource).copied().unwrap_or(0.0);
            }
            held + amount <= self.capacity(resource) + EPSILON
        })
    }

    /// Holds a node's claims for its `run`.
    pub(crate) fn acquire(&mut self, unit: usize) {
        if self.held.contains_key(&unit) {
            return;
        }
        self.held.insert(unit, Hold::Running);
        if let Some(claims) = self.claims.get(&unit) {
            add(&mut self.running, claims);
        }
    }

    /// Goes on holding a node's claims now its `run` has returned, for its group's guest work.
    pub(crate) fn linger(&mut self, unit: usize) {
        if self.held.get(&unit) != Some(&Hold::Running) {
            return;
        }
        self.held.insert(unit, Hold::Lingering);
        if let Some(claims) = self.claims.get(&unit) {
            take(&mut self.running, claims);
            add(&mut self.lingering, claims);
        }
    }

    /// Releases everything the node holds. Idempotent.
    pub(crate) fn release(&mut self, unit: usize) {
        let Some(hold) = self.held.remove(&unit) else {
            return;
        };
        if let Some(claims) = self.claims.get(&unit) {
            let from = match hold {
                Hold::Running => &mut self.running,
                Hold::Lingering => &mut self.lingering,
            };
            take(from, claims);
        }
    }

    #[cfg(test)]
    fn is_held(&self, unit: usize) -> bool {
        self.held.contains_key(&unit)
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
        assert!(pool.can_acquire(0, Room::Free));
        pool.acquire(0);
        assert!(pool.can_acquire(1, Room::Free), "there are two");
        pool.acquire(1);
        assert!(!pool.can_acquire(2, Room::Free), "and both are held");
        pool.release(0);
        assert!(pool.can_acquire(2, Room::Free));

        let mut small = ResourcePool::with_capacities(HashMap::from([(gpu.clone(), 0.5)]));
        assert_eq!(
            small.register(0, BTreeMap::from([(gpu.clone(), 0.8)])),
            Err(gpu)
        );
    }

    #[test]
    fn a_lingering_claim_keeps_out_all_but_what_is_let_past_it() {
        let mut pool = pool_with(&[(0, &[("gpu", 0.6)]), (1, &[("gpu", 0.6)])]);
        pool.acquire(0);
        assert!(!pool.can_acquire(1, Room::PastLingering), "a run in flight");
        pool.linger(0);
        assert!(!pool.can_acquire(1, Room::Free));
        assert!(pool.can_acquire(1, Room::PastLingering));
        pool.acquire(1);
        pool.release(1);
        assert!(!pool.can_acquire(1, Room::Free), "still lingering");
        pool.release(0);
        assert!(pool.can_acquire(1, Room::Free));
        pool.linger(0);
        assert!(!pool.is_held(0), "lingering takes a hold");
    }

    #[test]
    fn acquire_and_release() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)])]);
        assert!(pool.can_acquire(0, Room::Free));
        pool.acquire(0);
        assert!(pool.is_held(0));
        pool.release(0);
        assert!(!pool.is_held(0));
    }

    #[test]
    fn mutual_exclusion() {
        let mut pool = pool_with(&[(0, &[("disk", 0.6)]), (1, &[("disk", 0.6)])]);
        pool.acquire(0);
        assert!(!pool.can_acquire(1, Room::Free));
        pool.release(0);
        assert!(pool.can_acquire(1, Room::Free));
    }

    #[test]
    fn compatible_claims() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)]), (1, &[("disk", 0.5)])]);
        pool.acquire(0);
        assert!(pool.can_acquire(1, Room::Free));
        pool.acquire(1);
        assert!(pool.is_held(0) && pool.is_held(1));
    }

    #[test]
    fn no_claims_always_acquires() {
        assert!(ResourcePool::default().can_acquire(7, Room::Free));
    }

    #[test]
    fn release_and_acquire_are_idempotent() {
        let mut pool = pool_with(&[(0, &[("disk", 0.5)]), (1, &[("disk", 0.5)])]);
        pool.acquire(0);
        pool.acquire(0);
        assert!(
            pool.can_acquire(1, Room::Free),
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
        assert!(!pool.can_acquire(1, Room::Free));
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
