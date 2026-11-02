//! Saving a [`PreparedComponent`] and loading it back without compiling.
//!
//! A saved component is [`MAGIC`], the length of a JSON [`Saved`] as a
//! little-endian `u32`, that JSON, and wasmtime's serialized component,
//! which carries its own check of the wasmtime version and engine settings.

use serde::{Deserialize, Serialize};
use wasmtime::Engine;
use wasmtime::component::Component;
use witgraph_ir::{Capability, ComponentContract};
use witgraph_wit::hash::content_hash;

use super::{LinkKey, Parts, PreparedComponent, validate_engine};
use crate::error::LoadError;

/// What a saved component starts with; the last byte is the format's version.
const MAGIC: &[u8; 8] = b"witgrap\x01";

/// What a saved component holds besides its code: its [`Parts`], with the
/// contract reduced to its hash, as contracts are never serialized.
#[derive(Serialize, Deserialize)]
struct Saved {
    contract_hash: String,
    export: String,
    links: Vec<LinkKey>,
    capabilities: Vec<Capability>,
    components: usize,
}

impl PreparedComponent {
    /// The component as bytes for [`deserialize`](Self::deserialize) to load
    /// in another process, without its contract, which that process has
    /// again from its WIT.
    pub fn serialize(&self) -> Result<Vec<u8>, LoadError> {
        let bad = |what: &str, e: &dyn std::fmt::Display| LoadError::BadComponent {
            component: Box::new(self.parts.contract.id.clone()),
            message: format!("failed to save {what}: {e}"),
        };
        let parts = &self.parts;
        let saved = serde_json::to_vec(&Saved {
            contract_hash: content_hash(&parts.contract),
            export: parts.export.clone(),
            links: parts.links.clone(),
            capabilities: parts.capabilities.clone(),
            components: parts.components,
        })
        .map_err(|e| bad("its description", &e))?;
        let length = u32::try_from(saved.len()).map_err(|e| bad("its description", &e))?;
        let code = self
            .component
            .serialize()
            .map_err(|e| bad("its code", &format!("{e:#}")))?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + 4 + saved.len() + code.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&saved);
        bytes.extend_from_slice(&code);
        Ok(bytes)
    }

    /// Loads a component saved by [`serialize`](Self::serialize), for
    /// `engine` and `contract`, without compiling it or looking at its WIT.
    ///
    /// Bytes that are not a saved component, or whose code does not fit this
    /// wasmtime or `engine`'s settings, are a [`LoadError::BadComponent`]:
    /// engines made from different [`RuntimeConfig`](super::RuntimeConfig)s
    /// can differ. Bytes saved for another contract are a
    /// [`LoadError::ContractMismatch`]; an engine that cannot run a graph is
    /// an `InvalidConfig` ([`LoadError::Runtime`]).
    ///
    /// # Safety
    ///
    /// The bytes are run as native code, and nothing can check that code:
    /// see [`Component::deserialize`]. They must come from
    /// [`serialize`](Self::serialize), by way of storage only you can write to.
    pub unsafe fn deserialize(
        engine: &Engine,
        contract: &ComponentContract,
        bytes: &[u8],
    ) -> Result<Self, LoadError> {
        validate_engine(engine)?;
        let bad = |message: String| LoadError::BadComponent {
            component: Box::new(contract.id.clone()),
            message,
        };
        let not_saved = || bad("these bytes are not a saved component of this version".into());
        let rest = bytes.strip_prefix(MAGIC).ok_or_else(not_saved)?;
        let (length, rest) = rest.split_first_chunk::<4>().ok_or_else(not_saved)?;
        let length = u32::from_le_bytes(*length) as usize;
        let (saved, code) = rest.split_at_checked(length).ok_or_else(not_saved)?;
        let saved: Saved = serde_json::from_slice(saved).map_err(|_| not_saved())?;
        if saved.contract_hash != content_hash(contract) {
            return Err(LoadError::ContractMismatch {
                component: Box::new(contract.id.clone()),
                message: "it was saved for another contract".into(),
            });
        }
        // SAFETY: passed on to the caller.
        let component = unsafe { Component::deserialize(engine, code) }
            .map_err(|e| bad(format!("failed to load its code: {e:#}")))?;
        Ok(Self {
            parts: Parts {
                contract: contract.clone(),
                export: saved.export,
                links: saved.links,
                capabilities: saved.capabilities,
                components: saved.components,
            },
            component,
        })
    }
}
