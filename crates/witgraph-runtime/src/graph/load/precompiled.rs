//! Saving a [`PreparedGraph`] and loading it back without compiling.
//!
//! A saved graph is [`MAGIC`], the length of a JSON [`Saved`] as a
//! little-endian `u32`, that JSON, and wasmtime's serialized component,
//! which carries its own check of the wasmtime version and engine settings.

#[cfg(feature = "compile")]
use std::collections::HashMap;
#[cfg(feature = "compile")]
use std::fs::{self, File};
#[cfg(feature = "compile")]
use std::io::{self, Write};
#[cfg(feature = "compile")]
use std::path::{Path, PathBuf};
#[cfg(feature = "compile")]
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
#[cfg(feature = "compile")]
use sha2::{Digest, Sha256};
use wasmtime::Engine;
use wasmtime::component::Component;
#[cfg(feature = "compile")]
use witgraph_ir::ComponentRef;
use witgraph_ir::{Capability, CompiledGraph, NodeId};

use super::{Parts, PreparedGraph, fingerprint, validate_engine};
use crate::error::LoadError;

/// What a saved graph starts with; the last byte is the format's version.
const MAGIC: &[u8; 8] = b"witgrap\x02";

/// What a saved graph holds besides its code: its [`Parts`].
#[derive(Serialize, Deserialize)]
struct Saved {
    graph: String,
    capabilities: Vec<(NodeId, Vec<Capability>)>,
    components: usize,
    source: Option<String>,
}

impl PreparedGraph {
    /// The graph as bytes for [`deserialize`](Self::deserialize) to load in
    /// another process.
    ///
    /// # Compatibility
    ///
    /// The bytes are native code, for this build and machine only. They load
    /// only with the exact same wasmtime version (`50.0.0` does not load
    /// `50.0.1`'s), architecture and operating system, compiler settings,
    /// and engine settings, on a CPU with the features they were compiled to
    /// use. So keep the components' `.wasm` as the source, and treat these
    /// bytes as a build output: updating wasmtime makes them stale.
    pub fn serialize(&self) -> Result<Vec<u8>, LoadError> {
        let bad = |what: &str, e: &dyn std::fmt::Display| LoadError::GraphMismatch {
            message: format!("failed to save {what}: {e}"),
        };
        let parts = &self.parts;
        let saved = serde_json::to_vec(&Saved {
            graph: parts.graph.clone(),
            capabilities: parts.capabilities.clone(),
            components: parts.components,
            source: parts.source.clone(),
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

    /// Loads a graph saved by [`serialize`](Self::serialize), for `engine` and
    /// `compiled`, without compiling it or looking at any WIT.
    ///
    /// Bytes that are not a saved graph, whose code does not fit this wasmtime
    /// or `engine`'s settings (see [`serialize`](Self::serialize)), or saved for
    /// another graph are a [`LoadError::GraphMismatch`]: engines made from
    /// different [`RuntimeConfig`](super::RuntimeConfig)s can differ. An engine
    /// that cannot run a graph is an `InvalidConfig` ([`LoadError::Runtime`]).
    ///
    /// # Safety
    ///
    /// The bytes are run as native code, and nothing can check that code:
    /// see [`Component::deserialize`]. The compatibility checks catch
    /// mistakes, not tampering. The bytes must come from
    /// [`serialize`](Self::serialize), by way of storage only you can write to.
    pub unsafe fn deserialize(
        engine: &Engine,
        compiled: &CompiledGraph,
        bytes: &[u8],
    ) -> Result<Self, LoadError> {
        validate_engine(engine)?;
        let bad = |message: String| LoadError::GraphMismatch { message };
        let not_saved = || bad("these bytes are not a saved graph of this version".into());
        let rest = bytes.strip_prefix(MAGIC).ok_or_else(not_saved)?;
        let (length, rest) = rest.split_first_chunk::<4>().ok_or_else(not_saved)?;
        let length = u32::from_le_bytes(*length) as usize;
        let (saved, code) = rest.split_at_checked(length).ok_or_else(not_saved)?;
        let saved: Saved = serde_json::from_slice(saved).map_err(|_| not_saved())?;
        if saved.graph != fingerprint(compiled) {
            return Err(bad("it was saved for another graph".into()));
        }
        // SAFETY: passed on to the caller.
        let component = unsafe { Component::deserialize(engine, code) }
            .map_err(|e| bad(format!("failed to load its code: {e:#}")))?;
        Ok(Self {
            parts: Parts {
                graph: saved.graph,
                capabilities: saved.capabilities,
                components: saved.components,
                source: saved.source,
            },
            component,
        })
    }
}

#[cfg(feature = "compile")]
impl PreparedGraph {
    /// Loads the graph saved at `path` if it is still good, and otherwise
    /// prepares it from `wasm` and saves the result there.
    ///
    /// A file is still good if [`deserialize`](Self::deserialize) accepts it and
    /// it was prepared from these very bytes, which a changed component with the
    /// same contract is not. A file that is missing, unreadable, damaged or stale
    /// (see [`serialize`](Self::serialize)'s compatibility) is replaced, in one
    /// step, so a process reading it never sees half a file.
    ///
    /// Preparing reports what [`new`](Self::new) reports. An engine that cannot
    /// run a graph is an `InvalidConfig` ([`LoadError::Runtime`]). A result that
    /// cannot be saved is a [`LoadError::GraphMismatch`], rather than a graph that
    /// silently compiles on every start. Like `new` it runs synchronously, and it
    /// can take as long.
    ///
    /// # Safety
    ///
    /// As for [`deserialize`](Self::deserialize): the file at `path` is run
    /// as native code, so only you may be able to write to it.
    pub unsafe fn load_or_compile(
        engine: &Engine,
        compiled: &CompiledGraph,
        wasm: &HashMap<ComponentRef, &[u8]>,
        path: impl AsRef<Path>,
    ) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let source = source_hash(wasm);
        if let Ok(saved) = fs::read(path) {
            // SAFETY: passed on to the caller.
            match unsafe { Self::deserialize(engine, compiled, &saved) } {
                Ok(prepared) if prepared.parts.source.as_ref() == Some(&source) => {
                    return Ok(prepared);
                }
                // Compiling on this engine would fail too.
                Err(error @ LoadError::Runtime(_)) => return Err(error),
                Ok(_) | Err(_) => {}
            }
        }
        let prepared = Self::new(engine, compiled, wasm)?;
        save(path, &prepared.serialize()?).map_err(|e| LoadError::GraphMismatch {
            message: format!("failed to save it to `{}`: {e}", path.display()),
        })?;
        Ok(prepared)
    }
}

/// A hash of what a graph is prepared from: every component's key and bytes, in
/// any order.
#[cfg(feature = "compile")]
pub(super) fn source_hash(wasm: &HashMap<ComponentRef, &[u8]>) -> String {
    let mut hash = Sha256::new();
    let mut field = |field: &[u8]| {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field);
    };
    let mut keys: Vec<(String, &[u8])> = wasm
        .iter()
        .map(|(key, bytes)| (key.to_string(), *bytes))
        .collect();
    keys.sort();
    for (key, bytes) in keys {
        field(key.as_bytes());
        field(bytes);
    }
    hex::encode(hash.finalize())
}

/// Writes `bytes` to `path` in one step: to a file beside it, then renamed.
#[cfg(feature = "compile")]
fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = PathBuf::from(temporary);
    let written = File::create(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}
