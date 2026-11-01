//! Composing the providers a graph's links name into the nodes that use
//! them, before the loader transpiles anything.
//!
//! A JavaScript host runs a node as the single component jco transpiled, so
//! a link has to be resolved into that component's bytes: the same
//! composition the wasmtime runtime does
//! ([`witgraph_wit::compose::link_node`]), once per node component and set
//! of links.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use wasm_bindgen::JsError;
use witgraph_ir::{Capability, CompiledGraph, ComponentRef, Link, NodeId};
use witgraph_wit::compose::{LinkedProvider, imported_as, link_node};
use witgraph_wit::lower::Lowered;

use crate::graph::{error, hex};

/// A node component with its links composed in.
pub struct Composed {
    /// The composed component.
    pub bytes: Vec<u8>,
    /// Its SHA-256, in hex.
    pub sha256: String,
    /// What the composed component imports, as the host is asked for it:
    /// the node's unlinked imports and what its providers import.
    pub capabilities: Vec<Capability>,
}

/// The compositions of a graph.
#[derive(Default)]
pub struct Linked {
    /// Each distinct composition.
    pub composed: Vec<Composed>,
    /// Per node whose links apply, the composition it runs.
    pub of_node: HashMap<NodeId, usize>,
}

/// What identifies a link among a node's: its import, provider and export.
type LinkKey = (String, ComponentRef, String);

/// The bytes of the component a link names as its provider, among the
/// `components` entries `load` was given. A provider has no contract for a
/// content hash to name, so a pinned ref takes only the entry with that
/// very ref. An unpinned one takes the entry with its own ref, else the one
/// pinned entry of its world.
fn provider_bytes<'e>(
    entries: &'e [(ComponentRef, Vec<u8>)],
    provider: &ComponentRef,
) -> Result<&'e [u8], String> {
    if let Some((_, bytes)) = entries.iter().find(|(id, _)| id == provider) {
        return Ok(bytes);
    }
    let missing = || format!("no component `{provider:#}` for the provider of a link");
    if provider.content_hash.is_some() {
        return Err(missing());
    }
    let mut pinned = entries.iter().filter(|(id, _)| id.matches(provider));
    match (pinned.next(), pinned.next()) {
        (None, _) => Err(missing()),
        (Some((_, bytes)), None) => Ok(bytes),
        (Some((first, _)), Some((second, _))) => Err(format!(
            "the provider `{provider}` of a link is ambiguous: `{first:#}` and `{second:#}` both fit"
        )),
    }
}

/// Composes each node's links into its component. `entries` are the
/// graph's components as `load` was given them, `of_node` the index of the
/// one each node runs.
///
/// A node with no link that applies (its code never uses the import) is
/// left out of the result: it runs as it is.
pub fn compose_links(
    compiled: &CompiledGraph,
    entries: &[(ComponentRef, Vec<u8>)],
    of_node: &HashMap<NodeId, u32>,
) -> Result<Linked, wasm_bindgen::JsError> {
    let mut linked = Linked::default();
    // Each component and set of links already composed, and where; a
    // `None` is a set none of whose links applies.
    let mut done: HashMap<(&ComponentRef, Vec<LinkKey>), Option<usize>> = HashMap::new();
    let mut lowered: HashMap<&ComponentRef, Lowered> = HashMap::new();
    let mut decodable = HashSet::new();
    for node in &compiled.graph().nodes {
        let mut links: Vec<&Link> = compiled
            .graph()
            .links
            .iter()
            .filter(|link| link.node == node.id)
            .collect();
        if links.is_empty() {
            continue;
        }
        let contract = compiled
            .contract_for(&node.id)
            .ok_or_else(|| error(format!("unknown node `{}`", node.id)))?;
        links.sort_by(|a, b| {
            (&a.import, &a.provider, &a.export).cmp(&(&b.import, &b.provider, &b.export))
        });
        let key = (
            &contract.id,
            links
                .iter()
                .map(|l| (l.import.clone(), l.provider.clone(), l.export.clone()))
                .collect::<Vec<_>>(),
        );
        if let Some(&index) = done.get(&key) {
            if let Some(index) = index {
                linked.of_node.insert(node.id.clone(), index);
            }
            continue;
        }
        let bytes = of_node
            .get(&node.id)
            .and_then(|&index| entries.get(index as usize))
            .map(|(_, bytes)| bytes.as_slice())
            .ok_or_else(|| error(format!("node `{}` has no component", node.id)))?;
        if !lowered.contains_key(&contract.id) {
            let view = witgraph_wit::lower_component(bytes, &contract.id).map_err(error)?;
            lowered.insert(&contract.id, view);
        }
        let view = &lowered[&contract.id];
        // A link whose import the bytes never use is left out, so its
        // provider need not be there.
        let providers = links
            .iter()
            .map(|link| {
                let bytes = match imported_as(view, &link.import) {
                    Some(_) => provider_bytes(entries, &link.provider)
                        .map_err(|e| error(format!("component `{}`: {e}", contract.id)))?,
                    None => &[][..],
                };
                Ok(LinkedProvider {
                    import: &link.import,
                    provider: &link.provider,
                    bytes,
                    export: &link.export,
                })
            })
            .collect::<Result<Vec<_>, JsError>>()?;
        let result = link_node(contract, bytes, view, &providers, &mut decodable)
            .map_err(|e| error(format!("component `{}`: {e}", contract.id)))?;
        let index = match result.bytes {
            std::borrow::Cow::Borrowed(_) => None,
            std::borrow::Cow::Owned(bytes) => {
                let digest: [u8; 32] = Sha256::digest(&bytes).into();
                linked.composed.push(Composed {
                    bytes,
                    sha256: hex(&digest),
                    capabilities: result.capabilities,
                });
                Some(linked.composed.len() - 1)
            }
        };
        if let Some(index) = index {
            linked.of_node.insert(node.id.clone(), index);
        }
        done.insert(key, index);
    }
    Ok(linked)
}
