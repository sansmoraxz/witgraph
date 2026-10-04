//! Composing a node's component with the providers its links name.
//!
//! A [`Link`](witgraph_ir::Link) satisfies one of a node's imports with a
//! provider's export. That is component composition, and `wac-graph` does
//! it: the node and each provider are instantiated inside one outer
//! component, each linked import is given the provider's export as its
//! argument, and the node's `node` interface is exported again. `wac-graph`
//! also checks that an export fits the import it is given, and turns every
//! import nothing satisfies (the node's unlinked ones, and the providers'
//! own) into an import of the outer component: the capabilities the host
//! still has to provide.

use wac_graph::types::Package;
use wac_graph::{CompositionGraph, EncodeOptions, NodeId};
use witgraph_ir::ComponentRef;

/// Whether two components are the same bytes: the same slice, or equal.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    std::ptr::eq(a, b) || (a.len() == b.len() && a == b)
}

/// One link of a node, with the provider's bytes.
#[derive(Clone, Copy)]
pub struct LinkedProvider<'a> {
    /// The node's import the link satisfies, as its contract names it.
    pub import: &'a str,
    /// The component providing it.
    pub provider: &'a ComponentRef,
    /// The provider's encoded component.
    pub bytes: &'a [u8],
    /// The provider's export that satisfies the import.
    pub export: &'a str,
}

/// Not the provider's bytes, only how many there are.
impl std::fmt::Debug for LinkedProvider<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedProvider")
            .field("import", &self.import)
            .field("provider", &self.provider)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("export", &self.export)
            .finish()
    }
}

/// Why a composition failed: the import whose link is at fault (the first
/// link's when the whole composition is), and the reason.
pub(super) type ComposeError = (String, String);

/// A node composed with its providers.
pub(super) struct Composed {
    /// The composed component.
    pub(super) bytes: Vec<u8>,
    /// How many provider instances it holds.
    pub(super) providers: usize,
}

/// Composes `node` with its providers. Each link comes with the name the
/// node's bytes import it under (the contract's name, or the newer
/// semver-compatible version wit-component merged it to). `node_export` is
/// the name the node exports its `node` interface under; the composed
/// component exports it under the same name. Links naming the same
/// provider (refs that [match](ComponentRef::matches)) with the same bytes
/// share one instance of it; any other link gets an instance of its own,
/// so a link never runs on bytes resolved for another.
pub(super) fn compose(
    node: &[u8],
    node_export: &str,
    links: &[(&LinkedProvider<'_>, &str)],
) -> Result<Composed, ComposeError> {
    let whole = |message: String| {
        let import = links.first().map_or("", |(link, _)| link.import);
        (import.to_string(), message)
    };
    let mut graph = CompositionGraph::new();
    let instantiate = |graph: &mut CompositionGraph, name: &str, bytes: &[u8]| {
        let package = Package::from_bytes(name, None, bytes, graph.types_mut())
            .map_err(|e| format!("{e:#}"))?;
        let package = graph
            .register_package(package)
            .map_err(|e| format!("{e:#}"))?;
        Ok::<NodeId, String>(graph.instantiate(package))
    };
    let consumer = instantiate(&mut graph, "witgraph:node", node).map_err(whole)?;

    let mut providers: Vec<(&ComponentRef, &[u8], NodeId)> = Vec::new();
    for (link, argument) in links {
        let at = |message: String| (link.import.to_string(), message);
        // Bytes are compared only for matching refs, and only when they
        // are not the same slice: a byte-by-byte comparison is the rare
        // case of one provider loaded twice.
        let same = |(id, bytes, _): &&(&ComponentRef, &[u8], NodeId)| {
            id.matches(link.provider) && same_bytes(bytes, link.bytes)
        };
        let provider = match providers.iter().find(same) {
            Some((_, _, instance)) => *instance,
            None => {
                let name = format!("witgraph:provider-{}", providers.len());
                let instance = instantiate(&mut graph, &name, link.bytes)
                    .map_err(|e| at(format!("provider `{}`: {e}", link.provider)))?;
                providers.push((link.provider, link.bytes, instance));
                instance
            }
        };
        let export = graph
            .alias_instance_export(provider, link.export)
            .map_err(|e| at(format!("provider `{}`: {e:#}", link.provider)))?;
        graph
            .set_instantiation_argument(consumer, argument, export)
            .map_err(|e| {
                at(format!(
                    "it cannot take `{}` of `{}`: {e:#}",
                    link.export, link.provider
                ))
            })?;
    }

    let exported = graph
        .alias_instance_export(consumer, node_export)
        .map_err(|e| whole(format!("{e:#}")))?;
    graph
        .export(exported, node_export)
        .map_err(|e| whole(format!("{e:#}")))?;
    let bytes = graph
        // Not validated here: lowering the result validates it, and so does
        // compiling it.
        .encode(EncodeOptions {
            validate: false,
            ..EncodeOptions::default()
        })
        .map_err(|e| whole(format!("{e:#}")))?;
    Ok(Composed {
        bytes,
        providers: providers.len(),
    })
}
