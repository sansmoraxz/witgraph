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
//! still has to provide.

use std::borrow::Cow;

use wac_graph::types::Package;
use wac_graph::{CompositionGraph, EncodeOptions, NodeId};
use witgraph_ir::{Capability, ComponentContract, ComponentRef, LinkKey};

use crate::lower::Lowered;
use crate::lower_component;

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

/// Why no bytes could be found for a link's provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// None of the components given fits it.
    Missing,
    /// Several do, sorted.
    Ambiguous(Vec<ComponentRef>),
}

/// The bytes of the component a link names as its provider, among `components`. A
/// provider has no contract for a content hash to name, so a pinned ref takes only
/// the component with that very ref. An unpinned one takes the component with its
/// own ref, else the one pinned component of its world.
pub fn find_provider<'a, 'i>(
    components: impl IntoIterator<Item = (&'i ComponentRef, &'a [u8])>,
    provider: &ComponentRef,
) -> Result<&'a [u8], ProviderError> {
    let mut fits = Vec::new();
    for (id, bytes) in components {
        if id == provider {
            return Ok(bytes);
        }
        if id.matches(provider) {
            fits.push((id, bytes));
        }
    }
    if provider.content_hash.is_some() {
        return Err(ProviderError::Missing);
    }
    match fits.as_slice() {
        [] => Err(ProviderError::Missing),
        [(_, bytes)] => Ok(bytes),
        several => {
            let mut candidates: Vec<ComponentRef> =
                several.iter().map(|(id, _)| (*id).clone()).collect();
            candidates.sort();
            Err(ProviderError::Ambiguous(candidates))
        }
    }
}

/// A node's links with their providers' bytes, which `bytes` finds. A link whose
/// import the node's bytes never use is left with none, so its provider need not
/// be there.
pub fn linked_providers<'a, E>(
    lowered: &Lowered,
    links: &'a [LinkKey],
    mut bytes: impl FnMut(&ComponentRef) -> Result<&'a [u8], E>,
) -> Result<Vec<LinkedProvider<'a>>, E> {
    links
        .iter()
        .map(|(import, provider, export)| {
            let found = match imported_as(lowered, import) {
                Some(_) => bytes(provider)?,
                None => &[][..],
            };
            Ok(LinkedProvider {
                import,
                provider,
                bytes: found,
                export,
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
/// provider (refs that [match](ComponentRef::matches)) with the same bytes
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

/// The name component bytes (as `lowered` from them) import a contract's
/// `import` under: itself, or the newer semver-compatible version
/// wit-component merged it into. `None` when the bytes never import it.
pub fn imported_as<'l>(lowered: &'l Lowered, import: &str) -> Option<&'l str> {
    witgraph_ir::interface::resolve_import(&lowered.contract.capabilities, import)
        .filter(|c| c.is_interface())
        .map(|c| c.interface.as_str())
}

/// What the host is asked for and links: each capability in `imported`,
/// named as `contract` declares it when the node's own bytes (`lowered`)
/// import it under a newer semver-compatible version (wit-component
/// merges a dependency's newer pin into one import). Verification proved
/// every item the bytes use is in the declared version, so whatever serves
/// it serves them, and the linker resolves the newer import to it. What
/// only a composed provider imports is asked for as the provider imports
/// it.
fn as_declared(
    contract: &ComponentContract,
    lowered: &Lowered,
    imported: &[Capability],
) -> Vec<Capability> {
    let own = |name: &str| {
        lowered
            .contract
            .capabilities
            .iter()
            .any(|c| c.interface == name)
    };
    imported
        .iter()
        .map(|capability| {
            own(&capability.interface)
                .then(|| {
                    witgraph_ir::interface::resolve_import(
                        &contract.capabilities,
                        &capability.interface,
                    )
                })
                .flatten()
                .filter(|declared| declared.is_interface() == capability.is_interface())
                .unwrap_or(capability)
                .clone()
        })
        .collect()
}

/// Composes `links` into `bytes`, which implement `contract` as `lowered`
/// (from [`lower_component`]) says.
///
/// A link whose import the bytes do not have (their code never uses it)
/// has nothing to satisfy and is left out; one the bytes import at a newer
/// semver-compatible version (wit-component merges such imports) satisfies
/// that import. With no link left, `bytes` come back as they are.
pub fn link_node<'a>(
    contract: &ComponentContract,
    bytes: &'a [u8],
    lowered: &Lowered,
    links: &[LinkedProvider<'_>],
) -> Result<LinkedNode<'a>, LinkError> {
    let bad_link = |import: &str, message: String| LinkError {
        import: import.to_string(),
        message,
    };
    // Each link with the name the bytes import it under.
    let mut used: Vec<(&LinkedProvider<'_>, &str)> = Vec::with_capacity(links.len());
    for link in links {
        let Some(argument) = imported_as(lowered, link.import) else {
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
            export: lowered.export.clone(),
            capabilities: as_declared(contract, lowered, &lowered.contract.capabilities),
            components: 1,
        });
    }
    let export = lowered.export.clone();
    let composed =
        compose(bytes, &export, &used).map_err(|(import, message)| bad_link(&import, message))?;
    // The composed component's own view of its world: its ports are the
    // node's, its imports what is left for the host. The node's own bytes
    // lowered, so what fails now came with the providers (what they import).
    let with_providers = |message: String| {
        let import = used.first().map_or("", |(link, _)| link.import);
        bad_link(import, format!("with its providers composed in: {message}"))
    };
    let composed_view = lower_component(&composed.bytes, &contract.id)
        .map_err(|e| with_providers(e.to_string()))?;
    Ok(LinkedNode {
        bytes: Cow::Owned(composed.bytes),
        export,
        capabilities: as_declared(contract, lowered, &composed_view.contract.capabilities),
        components: 1 + composed.providers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> ComponentRef {
        text.parse().unwrap()
    }

    fn pinned(text: &str, hash: &str) -> ComponentRef {
        ComponentRef {
            content_hash: Some(hash.into()),
            ..id(text)
        }
    }

    #[test]
    fn a_provider_is_found_by_its_own_ref_first() {
        let (plain, hashed) = (id("a:b/c@1.0.0"), pinned("a:b/c@1.0.0", "aa"));
        let components = [(&hashed, &b"pinned"[..]), (&plain, &b"plain"[..])];
        assert_eq!(find_provider(components, &plain), Ok(&b"plain"[..]));
        assert_eq!(find_provider(components, &hashed), Ok(&b"pinned"[..]));
    }

    #[test]
    fn an_unpinned_provider_takes_the_one_pinned_component_of_its_world() {
        let (wanted, hashed) = (id("a:b/c@1.0.0"), pinned("a:b/c@1.0.0", "aa"));
        assert_eq!(
            find_provider([(&hashed, &b"pinned"[..])], &wanted),
            Ok(&b"pinned"[..])
        );
    }

    #[test]
    fn a_pinned_provider_takes_nothing_else() {
        let wanted = pinned("a:b/c@1.0.0", "bb");
        let (plain, other) = (id("a:b/c@1.0.0"), pinned("a:b/c@1.0.0", "aa"));
        let components = [(&plain, &b"plain"[..]), (&other, &b"other"[..])];
        assert_eq!(
            find_provider(components, &wanted),
            Err(ProviderError::Missing)
        );
    }

    #[test]
    fn another_version_is_no_provider() {
        let (wanted, other) = (id("a:b/c@1.0.0"), id("a:b/c@1.0.1"));
        assert_eq!(
            find_provider([(&other, &b"x"[..])], &wanted),
            Err(ProviderError::Missing)
        );
    }

    #[test]
    fn several_pinned_components_are_ambiguous() {
        let wanted = id("a:b/c@1.0.0");
        let (first, second) = (pinned("a:b/c@1.0.0", "aa"), pinned("a:b/c@1.0.0", "bb"));
        let components = [(&second, &b"2"[..]), (&first, &b"1"[..])];
        assert_eq!(
            find_provider(components, &wanted),
            Err(ProviderError::Ambiguous(vec![
                first.clone(),
                second.clone()
            ]))
        );
    }
}
