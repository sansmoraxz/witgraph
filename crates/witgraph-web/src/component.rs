//! The components `load` and the editor are given: each `{ name, manifest,
//! wit, bytes? }`, its node manifest (as JSON), its WIT (a wasm-encoded
//! package) and, to run it, its bytes.
//!
//! What a component is comes from its manifest and WIT alone: its worlds'
//! contracts are lowered from the WIT, never read out of its bytes, which are
//! only checked to target the worlds nodes run of them.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use sha2::{Digest, Sha256};
use wasm_bindgen::prelude::*;
use witgraph_ir::{Capability, ComponentContract, ComponentName, ContractId, WitRef, WorldRef};
use witgraph_wit::load::WitSource;

use crate::graph::{error, text};

/// What of a node manifest a browser uses: its locations are the host's
/// business.
#[derive(serde::Deserialize)]
struct Manifest {
    wit: WitRef,
    worlds: Vec<WorldRef>,
    #[serde(default)]
    viewers: BTreeMap<WorldRef, BTreeMap<String, String>>,
}

/// A node world of a component, lowered.
#[derive(Clone)]
pub(crate) struct Lowered {
    pub(crate) contract: ComponentContract,
    /// The name its `node` interface is exported under.
    pub(crate) export: String,
    /// The named types its ports reach, for the editor's catalog.
    pub(crate) lowered: witgraph_wit::lower::Lowered,
}

/// What a component is, from its manifest and WIT.
pub(crate) struct Described {
    /// Its WIT.
    pub(crate) wit: WitSource,
    /// Each node world it provides, lowered.
    pub(crate) worlds: Vec<Lowered>,
    /// What every world it provides imports.
    pub(crate) capabilities: Vec<Capability>,
    /// What was wrong but did not stop it describing: viewers skipped.
    pub(crate) warnings: Vec<String>,
}

/// A component as given.
#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) name: ComponentName,
    pub(crate) described: Rc<Described>,
    /// Its bytes, when given: what runs.
    pub(crate) bytes: Option<Rc<Vec<u8>>>,
}

impl Entry {
    /// The lowered world of `id`, if this component provides it.
    pub(crate) fn world(&self, id: &ContractId) -> Option<&Lowered> {
        self.described.worlds.iter().find(|w| &w.contract.id == id)
    }

    /// Its bytes, which a component must have to run.
    pub(crate) fn bytes(&self) -> Result<&[u8], JsError> {
        self.bytes
            .as_deref()
            .map(Vec::as_slice)
            .ok_or_else(|| error(format!("component `{}` needs `bytes` to run", self.name)))
    }
}

/// What a description is made from: the component's name and the SHA-256 of
/// its manifest's JSON and of its WIT.
#[derive(PartialEq, Eq)]
struct Key {
    name: ComponentName,
    manifest: [u8; 32],
    wit: [u8; 32],
}

/// Bytes found to target a world: the SHA-256 of the bytes and of the WIT,
/// and the world.
type Targeted = ([u8; 32], [u8; 32], WorldRef);

/// The most descriptions kept for later loads.
const CACHED: usize = 32;

thread_local! {
    /// The descriptions the latest loads made, latest last: an editor that
    /// reloads its graph on every edit describes each component once.
    static DESCRIBED: RefCell<Vec<(Key, Rc<Described>)>> = const { RefCell::new(Vec::new()) };
    /// What bytes were found to target which worlds: the SHA-256 of the bytes
    /// and the WIT, and the world.
    static TARGETED: RefCell<Vec<Targeted>> = const { RefCell::new(Vec::new()) };
}

fn field(entry: &JsValue, name: &str) -> Option<JsValue> {
    Reflect::get(entry, &text(name))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
}

/// `value`'s bytes, if it is a `Uint8Array` (or another view). `isView` sees
/// through no proxy: what passes copies out without calling back into
/// JavaScript.
fn bytes_of(value: JsValue) -> Option<Vec<u8>> {
    Some(value)
        .filter(js_sys::ArrayBuffer::is_view)
        .and_then(|bytes| bytes.dyn_into::<Uint8Array>().ok())
        .map(|bytes| bytes.to_vec())
}

/// A component `load` or a checker is given.
pub(crate) fn entry(entry: &JsValue) -> Result<Entry, JsError> {
    let name: ComponentName = field(entry, "name")
        .and_then(|name| name.as_string())
        .ok_or_else(|| error("a component needs a `name`"))?
        .parse()
        .map_err(error)?;
    let manifest = field(entry, "manifest")
        .ok_or_else(|| error(format!("component `{name}` needs a `manifest`")))?;
    let manifest = match manifest.as_string() {
        Some(text) => text,
        None => js_sys::JSON::stringify(&manifest)
            .ok()
            .and_then(|text| text.as_string())
            .ok_or_else(|| error(format!("the manifest of `{name}` is not JSON")))?,
    };
    let wit = field(entry, "wit")
        .and_then(bytes_of)
        .ok_or_else(|| error(format!("component `{name}` needs its `wit`, encoded")))?;
    let bytes = field(entry, "bytes").and_then(bytes_of).map(Rc::new);
    let described = describe(&name, &manifest, &wit)?;
    Ok(Entry {
        name,
        described,
        bytes,
    })
}

/// What component `name` is, from its manifest and WIT: as a load described it
/// before, else now.
fn describe(name: &ComponentName, manifest: &str, wit: &[u8]) -> Result<Rc<Described>, JsError> {
    let key = Key {
        name: name.clone(),
        manifest: Sha256::digest(manifest).into(),
        wit: Sha256::digest(wit).into(),
    };
    let cached = DESCRIBED.with_borrow_mut(|cache| {
        let index = cache.iter().position(|(k, _)| *k == key)?;
        let entry = cache.remove(index);
        let described = entry.1.clone();
        cache.push(entry);
        Some(described)
    });
    if let Some(described) = cached {
        return Ok(described);
    }
    let described = Rc::new(described_now(name, manifest, wit)?);
    DESCRIBED.with_borrow_mut(|cache| {
        cache.push((key, described.clone()));
        if cache.len() > CACHED {
            cache.remove(0);
        }
    });
    Ok(described)
}

fn described_now(name: &ComponentName, manifest: &str, wit: &[u8]) -> Result<Described, JsError> {
    let failed = |message: String| error(format!("component `{name}`: {message}"));
    let manifest: Manifest =
        serde_json::from_str(manifest).map_err(|e| failed(format!("its manifest: {e}")))?;
    let wit = witgraph_wit::load::load_encoded(wit).map_err(|e| failed(format!("its WIT: {e}")))?;
    let mut worlds = Vec::new();
    let mut capabilities = Vec::new();
    let mut warnings = Vec::new();
    for world in &manifest.worlds {
        if world.package != manifest.wit {
            return Err(failed(format!(
                "world `{world}` is not in its package `{}`",
                manifest.wit
            )));
        }
        let id = witgraph_wit::lower::find_world(&wit, world)
            .ok_or_else(|| failed(format!("its WIT has no world `{world}`")))?;
        Capability::add_unique(
            &mut capabilities,
            &witgraph_wit::lower::world_capabilities(&wit, id),
        );
        match witgraph_wit::lower_world(&wit, &ContractId::new(name.clone(), world.clone())) {
            Ok(mut lowered) => {
                if let Some(hints) = manifest.viewers.get(world) {
                    warnings.extend(witgraph_wit::viewer::apply(&mut lowered.contract, hints));
                }
                worlds.push(Lowered {
                    contract: lowered.contract.clone(),
                    export: lowered.export.clone(),
                    lowered,
                });
            }
            Err(witgraph_wit::Error::NotANode { .. }) => {}
            Err(e) => return Err(failed(e.to_string())),
        }
    }
    Ok(Described {
        wit,
        worlds,
        capabilities,
        warnings,
    })
}

/// Checks that `entry`'s bytes target `world` of its WIT, once per bytes,
/// WIT and world.
pub(crate) fn check_targets(entry: &Entry, world: &WorldRef) -> Result<(), JsError> {
    let bytes = entry.bytes()?;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let wit: [u8; 32] =
        Sha256::digest(witgraph_wit::load::encode(&entry.described.wit).map_err(error)?).into();
    let seen = TARGETED.with_borrow(|seen| {
        seen.iter()
            .any(|(b, w, x)| *b == digest && *w == wit && x == world)
    });
    if seen {
        return Ok(());
    }
    witgraph_wit::verify::verify_component(&entry.described.wit, world, bytes)
        .map_err(|e| error(format!("component `{}`: {e}", entry.name)))?;
    TARGETED.with_borrow_mut(|seen| {
        seen.push((digest, wit, world.clone()));
        if seen.len() > CACHED {
            seen.remove(0);
        }
    });
    Ok(())
}

/// Describes a component as the editor catalog does: one entry per node world
/// it provides (`{ id, component, world, package, docs, inputs, outputs,
/// capabilities, types }`, a port being `{ name, kind, type_display, handle?,
/// optional, docs?, viewer? }`), from its `manifest` (JSON) and `wit` (an
/// encoded WIT package). No bytes are needed.
#[wasm_bindgen(js_name = describeComponent)]
pub fn describe_component(name: &str, manifest: JsValue, wit: &[u8]) -> Result<JsValue, JsError> {
    let object = js_sys::Object::new();
    Reflect::set(&object, &text("name"), &text(name)).map_err(|_| error("no object"))?;
    Reflect::set(&object, &text("manifest"), &manifest).map_err(|_| error("no object"))?;
    Reflect::set(&object, &text("wit"), &Uint8Array::from(wit)).map_err(|_| error("no object"))?;
    let entry = entry(&object)?;
    let lowered: Vec<witgraph_wit::lower::Lowered> = entry
        .described
        .worlds
        .iter()
        .map(|w| {
            let mut lowered = w.lowered.clone();
            lowered.contract = w.contract.clone();
            lowered
                .warnings
                .extend(entry.described.warnings.iter().cloned());
            lowered
        })
        .collect();
    let catalog = witgraph_wit::metadata::generate_catalog(&lowered);
    crate::graph::json(&catalog.components)
}

/// Encodes WIT text (one document; `name` is its pseudo-path in errors, and
/// packages it depends on are nested in it) as one wasm-encoded package: what
/// a component's `wit` is.
#[wasm_bindgen(js_name = encodeWit)]
pub fn encode_wit(name: &str, wit: &str) -> Result<Vec<u8>, JsError> {
    let source = witgraph_wit::load::load_str(name, wit).map_err(error)?;
    witgraph_wit::load::encode(&source).map_err(error)
}
