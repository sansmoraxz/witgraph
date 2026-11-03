//! Shared objects: what handle ports carry.
//!
//! A handle port ([`PortKind::Handle`](witgraph_ir::PortKind::Handle))
//! carries a resource the host provides. What is behind it is the
//! provider's: bytes in host memory, a GPU buffer, a tensor of an inference
//! runtime. witgraph only keeps it, as a [`Shared`]: a reference-counted
//! object it latches between runs, lends to each reader's `run` as a
//! `borrow`, and drops when no latch holds it any more, which is when the
//! object's own `Drop` frees what it holds.
//!
//! # Providing a shareable resource
//!
//! A capability provider (a [`Host`](crate::Host), or a
//! [`CapabilityPlugin`](crate::CapabilityPlugin)) that implements a
//! resource handle ports may carry:
//!
//! - says so ([`Host::shares`](crate::Host::shares),
//!   [`CapabilityPlugin::shares`](crate::CapabilityPlugin::shares)), so a
//!   graph whose ports carry a resource nothing shares fails to load
//!   ([`LoadError::NotShareable`](crate::LoadError::NotShareable));
//! - defines the resource with [`link_resource`], which gives it the type
//!   every node's handles of it have;
//! - makes objects with [`HostState::share`](crate::HostState::share),
//!   which charges their size to the graph's
//!   [`max_shared_memory`](crate::RuntimeConfig::max_shared_memory) and
//!   hands the guest an `own` handle;
//! - finds the object behind a handle its methods get with
//!   [`HostState::shared`](crate::HostState::shared).

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wasmtime::component::{LinkerInstance, ResourceDynamic, ResourceType};
use witgraph_ir::interface::compat_track;

use crate::engine::StoreData;

/// An object a handle port carries: cloning it clones a reference, and two
/// are equal only when they are the same object.
#[derive(Clone)]
pub struct Shared {
    inner: Arc<Inner>,
}

struct Inner {
    tag: u32,
    object: Box<dyn Any + Send + Sync>,
    bytes: usize,
    budget: Arc<SharedMemory>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl Shared {
    /// The object, if it is a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.inner.object.downcast_ref()
    }

    /// The size it was charged at.
    pub fn bytes(&self) -> usize {
        self.inner.bytes
    }

    /// The type of its handles.
    pub fn resource_type(&self) -> ResourceType {
        ResourceType::host_dynamic(self.inner.tag)
    }

    pub(crate) fn tag(&self) -> u32 {
        self.inner.tag
    }
}

impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("tag", &self.inner.tag)
            .field("bytes", &self.inner.bytes)
            .finish_non_exhaustive()
    }
}

/// The type every node's handles of `resource`, declared in `interface`
/// (a full interface id), have: one per resource, the same for every
/// version on one semver track, as connections match handles.
pub fn resource_type(interface: &str, resource: &str) -> ResourceType {
    ResourceType::host_dynamic(tag(interface, resource))
}

/// The resource's type tag: a hash of its interface's compatibility track
/// and its name (FNV-1a). A graph whose handle ports carry two resources
/// of one tag fails to load.
pub(crate) fn tag(interface: &str, resource: &str) -> u32 {
    let key = format!("{}#{resource}", compat_track(interface));
    key.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// Defines `resource`, declared in `interface`, in a node's linker instance
/// for `interface`, as a resource handle ports may carry. A guest dropping
/// an `own` handle of it drops its reference; the object lives on while
/// anything else holds it.
pub fn link_resource<D: StoreData>(
    instance: &mut LinkerInstance<'_, D>,
    interface: &str,
    resource: &str,
) -> wasmtime::Result<()> {
    instance.resource(
        resource,
        resource_type(interface, resource),
        |mut store, rep| {
            store.data_mut().host_state().shared_table().remove(rep);
            Ok(())
        },
    )
}

/// One Store's shared objects that its guests hold handles to, by `rep`.
#[derive(Debug, Default)]
pub(crate) struct SharedTable {
    slots: Vec<Option<Shared>>,
    free: Vec<u32>,
}

impl SharedTable {
    /// Holds `shared` for a handle; its `rep`.
    pub(crate) fn insert(&mut self, shared: Shared) -> wasmtime::Result<u32> {
        if let Some(rep) = self.free.pop() {
            self.slots[rep as usize] = Some(shared);
            return Ok(rep);
        }
        let rep = u32::try_from(self.slots.len())
            .map_err(|_| wasmtime::format_err!("too many shared objects in one Store"))?;
        self.slots.push(Some(shared));
        Ok(rep)
    }

    pub(crate) fn get(&self, rep: u32) -> Option<&Shared> {
        self.slots.get(rep as usize)?.as_ref()
    }

    pub(crate) fn remove(&mut self, rep: u32) -> Option<Shared> {
        let shared = self.slots.get_mut(rep as usize)?.take()?;
        self.free.push(rep);
        Some(shared)
    }
}

/// The graph's shared objects' memory, granted so far, against
/// [`max_shared_memory`](crate::RuntimeConfig::max_shared_memory).
#[derive(Debug)]
pub(crate) struct SharedMemory {
    used: AtomicUsize,
    limit: Option<usize>,
}

impl SharedMemory {
    pub(crate) fn new(limit: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    fn charge(&self, bytes: usize) -> Result<(), SharedLimitExceeded> {
        let limit = self.limit;
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let requested = used.saturating_add(bytes);
                limit
                    .is_none_or(|limit| requested <= limit)
                    .then_some(requested)
            })
            .map(|_| ())
            .map_err(|used| SharedLimitExceeded {
                requested: used.saturating_add(bytes),
                limit: limit.unwrap_or(usize::MAX),
            })
    }

    fn release(&self, bytes: usize) {
        let _ = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    /// Bytes held now.
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// A new object, charged.
    pub(crate) fn share<T: Any + Send + Sync>(
        self: &Arc<Self>,
        tag: u32,
        object: T,
        bytes: usize,
    ) -> Result<Shared, SharedLimitExceeded> {
        self.charge(bytes)?;
        Ok(Shared {
            inner: Arc::new(Inner {
                tag,
                object: Box::new(object),
                bytes,
                budget: self.clone(),
            }),
        })
    }
}

/// Making a shared object would take the graph past its
/// `max_shared_memory`.
#[derive(Debug)]
pub(crate) struct SharedLimitExceeded {
    pub(crate) requested: usize,
    pub(crate) limit: usize,
}

impl std::fmt::Display for SharedLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "shared objects need {} bytes, over the graph's limit of {}",
            self.requested, self.limit
        )
    }
}

impl std::error::Error for SharedLimitExceeded {}

/// The own handle a guest gets for `rep`, of `tag`.
pub(crate) fn own(rep: u32, tag: u32) -> ResourceDynamic {
    ResourceDynamic::new_own(rep, tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_follow_the_semver_track() {
        let tensor = |interface| tag(interface, "tensor");
        assert_eq!(
            tensor("test:data/tensor@0.1.0"),
            tensor("test:data/tensor@0.1.7")
        );
        assert_ne!(
            tensor("test:data/tensor@0.1.0"),
            tensor("test:data/tensor@0.2.0")
        );
        assert_ne!(
            tensor("test:data/tensor@0.1.0"),
            tag("test:data/tensor@0.1.0", "blob")
        );
    }

    #[test]
    fn objects_are_charged_until_the_last_reference_drops() {
        let budget = SharedMemory::new(Some(100));
        let a = budget.share(1, vec![0u8; 60], 60).unwrap();
        assert!(budget.share(1, (), 50).is_err(), "over the limit");
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, budget.share(1, (), 10).unwrap());
        drop(a);
        assert_eq!(budget.used(), 60, "`b` still holds it");
        assert_eq!(b.downcast_ref::<Vec<u8>>().map(Vec::len), Some(60));
        drop(b);
        assert_eq!(budget.used(), 0);

        let mut table = SharedTable::default();
        let one = table.insert(budget.share(1, (), 1).unwrap()).unwrap();
        let two = table.insert(budget.share(1, (), 2).unwrap()).unwrap();
        assert!(table.remove(one).is_some());
        assert!(table.get(one).is_none());
        assert_eq!(table.insert(budget.share(1, (), 3).unwrap()).unwrap(), one);
        assert_eq!(table.get(two).map(Shared::bytes), Some(2));
    }
}
