//! Composing the providers a graph's links name into the nodes that use
//! them, before the loader transpiles anything.
//!
//! A link has to be resolved into a node's bytes before the graph is composed: the same
//! composition the wasmtime runtime does
//! ([`witgraph_wit::compose::link_node`]), once per node component and set
//! of links.

use std::collections::HashMap;

use witgraph_ir::{Capability, CompiledGraph, ComponentRef, LinkKey, NodeId};
use witgraph_wit::compose::{ProviderError, find_provider, link_node, linked_providers};
use witgraph_wit::lower::Lowered;

use crate::graph::error;

/// A node component with its links composed in.
pub struct Composed {
    /// The composed component.
    pub bytes: Vec<u8>,
    /// What the composed component imports, as the host is asked for it:
    /// the node's unlinked imports and what its providers import.
    pub capabilities: Vec<Capability>,
    /// The name it exports its `node` interface under.
    pub export: String,
}

/// The compositions of a graph.
#[derive(Default)]
pub struct Linked {
    /// Each distinct composition.
    pub composed: Vec<Composed>,
    /// Per node whose links apply, the composition it runs.
    pub of_node: HashMap<NodeId, usize>,
}

/// The bytes of the component a link names as its provider, among the
/// `components` entries `load` was given.
fn provider_bytes<'e>(
    entries: &'e [(ComponentRef, Vec<u8>)],
    provider: &ComponentRef,
) -> Result<&'e [u8], String> {
    let components = entries.iter().map(|(id, bytes)| (id, bytes.as_slice()));
    find_provider(components, provider).map_err(|e| match e {
        ProviderError::Missing => format!("no component `{provider:#}` for the provider of a link"),
        ProviderError::Ambiguous(candidates) => {
            let candidates: Vec<String> = candidates.iter().map(|c| format!("`{c:#}`")).collect();
            format!(
                "the provider `{provider}` of a link is ambiguous: {} all fit",
                candidates.join(", ")
            )
        }
    })
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
    for node in &compiled.graph().nodes {
        let links = compiled.link_keys(&node.id);
        if links.is_empty() {
            continue;
        }
        let contract = compiled
            .contract_for(&node.id)
            .ok_or_else(|| error(format!("unknown node `{}`", node.id)))?;
        let key = (&contract.id, links);
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
        let providers = linked_providers(view, &key.1, |provider| {
            provider_bytes(entries, provider)
                .map_err(|e| error(format!("component `{}`: {e}", contract.id)))
        })?;
        let result = link_node(contract, bytes, view, &providers)
            .map_err(|e| error(format!("component `{}`: {e}", contract.id)))?;
        let index = match result.bytes {
            std::borrow::Cow::Borrowed(_) => None,
            std::borrow::Cow::Owned(bytes) => {
                linked.composed.push(Composed {
                    bytes,
                    capabilities: result.capabilities,
                    export: result.export,
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
