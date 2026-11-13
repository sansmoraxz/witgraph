//! Composing a node's component with the providers its links name.
//!
//! Behind the `compose` feature: it brings in `wac-graph`, which a host that
//! only reads contracts has no use for.
//!
//! A [`Link`](witgraph_ir::Link) satisfies one of a node's imports with a
//! provider's export. That is component composition, and `wac-graph` does
//! it: the node and each provider are instantiated inside one outer
//! component, each linked import is given the provider's export as its
//! argument, and the node's `node` interface is exported again. `wac-graph`
//! also checks that an export fits the import it is given, and turns every
//! import nothing satisfies (the node's unlinked ones, and the providers'
//! own) into an import of the outer component: the capabilities the host
//! still has to provide, as the node's and the providers' WIT declare them.

use std::borrow::Cow;

use wac_graph::types::Package;
use wac_graph::{CompositionGraph, EncodeOptions, NodeId};
use witgraph_ir::{Capability, ComponentContract, ComponentName, LinkKey};

use crate::verify::Imports;

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
    pub provider: &'a ComponentName,
    /// The provider's encoded component.
    pub bytes: &'a [u8],
    /// The provider's export that satisfies the import.
    pub export: &'a str,
    /// What the provider imports, as its WIT declares it: what composing it
    /// in asks the host for.
    pub capabilities: &'a [Capability],
}

/// A provider, as a node's links find it: its bytes, and what it imports.
pub type Provider<'a> = (&'a [u8], &'a [Capability]);

/// A node's links with their providers, which `find` finds. A link whose
/// import the node's bytes (which import `imports`) never use is left with
/// none, so its provider need not be there.
pub fn linked_providers<'a, E>(
    imports: &Imports,
    links: &'a [LinkKey],
    mut find: impl FnMut(&ComponentName) -> Result<Provider<'a>, E>,
) -> Result<Vec<LinkedProvider<'a>>, E> {
    links
        .iter()
        .map(|(import, provider, export)| {
            let (bytes, capabilities) = match imports.imported_as(import) {
                Some(_) => find(provider)?,
                None => (&[][..], &[][..]),
            };
            Ok(LinkedProvider {
                import,
                provider,
                bytes,
                export,
                capabilities,
            })
        })
        .collect()
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
type ComposeError = (String, String);

/// A node composed with its providers.
struct Composed {
    /// The composed component.
    bytes: Vec<u8>,
    /// How many provider instances it holds.
    providers: usize,
}

/// Composes `node` with its providers. Each link comes with the name the
/// node's bytes import it under (the contract's name, or the newer
/// semver-compatible version wit-component merged it to). `node_export` is
/// the name the node exports its `node` interface under; the composed
/// component exports it under the same name. Links naming the same
/// provider with the same bytes
/// share one instance of it; any other link gets an instance of its own,
/// so a link never runs on bytes resolved for another.
fn compose(
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

    let mut providers: Vec<(&ComponentName, &[u8], NodeId)> = Vec::new();
    for (link, argument) in links {
        let at = |message: String| (link.import.to_string(), message);
        // Bytes are compared only for one name, and only when they are
        // not the same slice: a byte-by-byte comparison is the rare case of
        // one provider loaded twice.
        let same = |(name, bytes, _): &&(&ComponentName, &[u8], NodeId)| {
            *name == link.provider && same_bytes(bytes, link.bytes)
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

/// Why a node's links could not be composed in: the import of the link at
/// fault (the first link's when the whole composition is) and the reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("link on `{import}`: {message}")]
pub struct LinkError {
    /// The import whose link is at fault.
    pub import: String,
    /// What is wrong with it.
    pub message: String,
}

/// A node with its links composed in.
#[derive(Debug)]
pub struct LinkedNode<'a> {
    /// The node's bytes, or the composed component's when a link applied.
    pub bytes: Cow<'a, [u8]>,
    /// The name the `node` interface is exported under, in `bytes`.
    pub export: String,
    /// What the host is asked for: the capabilities `bytes` import, named
    /// as the node's contract declares them. After composition that is the
    /// node's unlinked imports plus what its providers import.
    pub capabilities: Vec<Capability>,
    /// How many components `bytes` holds: the node and each provider
    /// instance.
    pub components: usize,
}

/// Composes `links` into `bytes`, which implement `contract`, export its
/// `node` interface as `export`, and import `imports`.
///
/// A link whose import the bytes do not have (their code never uses it)
/// has nothing to satisfy and is left out; one the bytes import at a newer
/// semver-compatible version (wit-component merges such imports) satisfies
/// that import. With no link left, `bytes` come back as they are.
pub fn link_node<'a>(
    contract: &ComponentContract,
    bytes: &'a [u8],
    export: &str,
    imports: &Imports,
    links: &[LinkedProvider<'_>],
) -> Result<LinkedNode<'a>, LinkError> {
    let bad_link = |import: &str, message: String| LinkError {
        import: import.to_string(),
        message,
    };
    // Each link with the name the bytes import it under.
    let mut used: Vec<(&LinkedProvider<'_>, &str)> = Vec::with_capacity(links.len());
    for link in links {
        let Some(argument) = imports.imported_as(link.import) else {
            continue;
        };
        // Two contract imports wit-component merged into one take one
        // link, as compiling a graph requires (`ImportLinkedTwice`).
        if let Some((other, _)) = used.iter().find(|(_, name)| *name == argument) {
            return Err(bad_link(
                link.import,
                format!(
                    "the bytes import it and `{}` as one, `{argument}`, which another link \
                     already satisfies",
                    other.import
                ),
            ));
        }
        used.push((link, argument));
    }
    if used.is_empty() {
        return Ok(LinkedNode {
            bytes: Cow::Borrowed(bytes),
            export: export.to_string(),
            capabilities: imports.used(&contract.capabilities),
            components: 1,
        });
    }
    let composed =
        compose(bytes, export, &used).map_err(|(import, message)| bad_link(&import, message))?;
    // What the composed component imports: the node's unlinked imports, as
    // its contract declares them, and its providers', as theirs do.
    let with_providers = |message: String| {
        let import = used.first().map_or("", |(link, _)| link.import);
        bad_link(import, format!("with its providers composed in: {message}"))
    };
    let left = Imports::of(&composed.bytes).map_err(|e| with_providers(e.to_string()))?;
    let mut declared = contract.capabilities.clone();
    for (link, _) in &used {
        Capability::add_unique(&mut declared, link.capabilities);
    }
    Ok(LinkedNode {
        bytes: Cow::Owned(composed.bytes),
        export: export.to_string(),
        capabilities: left.used(&declared),
        components: 1 + composed.providers,
    })
}
