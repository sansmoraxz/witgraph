//! What loading a graph's components can fail with ([`LoadError`]),
//! besides the scheduler's [`RuntimeError`].

use witgraph_ir::{ComponentRef, NodeId, PortName};
use witgraph_sched::RuntimeError;

/// Why a graph could not be loaded: its components, their links and
/// capabilities, or instantiating it; or a [`RuntimeError`] (an
/// invalid config, say).
#[derive(Debug, Clone, thiserror::Error, miette::Diagnostic)]
pub enum LoadError {
    /// A link names its provider without a content hash, and several
    /// pinned keys of the WASM map match it: which bytes to compose in is
    /// not known.
    #[error(
        "provider `{provider}` matches several components: {}; pin it, or key one without a hash",
        candidates.iter().map(|c| format!("`{c:#}`")).collect::<Vec<_>>().join(", ")
    )]
    #[diagnostic(code(witgraph::load::ambiguous_provider))]
    AmbiguousProvider {
        /// The provider as the link names it.
        provider: Box<ComponentRef>,
        /// The keys that match it.
        candidates: Vec<ComponentRef>,
    },
    /// A component has no corresponding WASM bytes.
    #[error(
        "no WASM component provided for `{component}`{}",
        component.content_hash.as_ref().map(|h| format!(" (content hash {h})")).unwrap_or_default()
    )]
    #[diagnostic(code(witgraph::load::missing_wasm))]
    MissingWasm {
        /// The component missing its bytes.
        component: Box<ComponentRef>,
    },
    /// A component's bytes are not a valid component, embed WIT that could
    /// not be decoded or lowered into exactly one node contract, or failed
    /// to compile on the engine.
    #[error("component `{component}` does not describe a witgraph node: {message}")]
    #[diagnostic(code(witgraph::load::bad_component))]
    BadComponent {
        /// The component whose bytes were rejected.
        component: Box<ComponentRef>,
        /// What went wrong.
        message: String,
    },
    /// A component's bytes implement a different contract than the one the
    /// graph was compiled against: other ports or `run` kind, or an import
    /// the contract does not declare as a capability.
    #[error("component `{component}` does not match its contract: {message}")]
    #[diagnostic(code(witgraph::load::contract_mismatch))]
    ContractMismatch {
        /// The component whose bytes were rejected.
        component: Box<ComponentRef>,
        /// The first difference found.
        message: String,
    },
    /// A [`Link`](witgraph_ir::Link) could not be made: the provider has no
    /// such export, the export does not fit the node's import, or the node
    /// and its providers do not compose.
    #[error("component `{component}` cannot link import `{import}`: {message}")]
    #[diagnostic(code(witgraph::load::bad_link))]
    BadLink {
        /// The node's component.
        component: Box<ComponentRef>,
        /// The import the link satisfies.
        import: String,
        /// What went wrong.
        message: String,
    },
    /// The graph could not be instantiated (a missing import the host did not
    /// link, a trapping start function, a Store over its memory limit), or the
    /// host failed to make the data of its Store.
    #[error("failed to instantiate the graph: {message}")]
    #[diagnostic(code(witgraph::load::instantiation))]
    Instantiation {
        /// The node that failed, when it is known: one that called `fatal` from a
        /// start function. A composed graph instantiates as one.
        node: Option<NodeId>,
        /// The underlying error message.
        message: String,
    },
    /// The graph could not be composed with its generated hub.
    #[error("failed to compose the graph: {message}")]
    #[diagnostic(code(witgraph::load::composition))]
    Composition {
        /// What went wrong.
        message: String,
    },
    /// A prepared graph was given for another graph.
    #[error("the prepared graph does not fit: {message}")]
    #[diagnostic(code(witgraph::load::graph_mismatch))]
    GraphMismatch {
        /// What differs.
        message: String,
    },
    /// A node imports a capability the host does not provide
    /// ([`Host::check`](crate::Host::check)). Reported before anything is
    /// linked.
    #[error(
        "node `{node}` imports capability `{capability}`{}, which the host does not provide",
        implements.as_ref().map(|i| format!(" (`{i}`)")).unwrap_or_default()
    )]
    #[diagnostic(code(witgraph::load::missing_capability))]
    MissingCapability {
        /// The node importing it.
        node: NodeId,
        /// The capability, as the node imports it.
        capability: String,
        /// For a labelled import, the interface the label stands for.
        implements: Option<String>,
    },
    /// Several providers could serve a capability a node imports, and
    /// nothing picks one.
    #[error(
        "node `{node}` imports capability `{capability}`{}, which {} could each provide; {}",
        implements.as_ref().map(|i| format!(" (`{i}`)")).unwrap_or_default(),
        providers.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "),
        ambiguity_fix(capability, implements.is_some())
    )]
    #[diagnostic(code(witgraph::load::ambiguous_capability))]
    AmbiguousCapability {
        /// The node importing it.
        node: NodeId,
        /// The capability, as the node imports it.
        capability: String,
        /// For a labelled import, the interface the label stands for.
        implements: Option<String>,
        /// The ids of the providers that could serve it.
        providers: Vec<String>,
    },
    /// A handle port carries a resource the host does not share
    /// ([`Host::shares`](crate::Host::shares)): one whose handles cannot
    /// pass from node to node.
    #[error("`{node}.{port}` carries `{resource}`, which the host does not share between nodes")]
    #[diagnostic(code(witgraph::load::not_shareable))]
    NotShareable {
        /// The node.
        node: NodeId,
        /// The handle port.
        port: PortName,
        /// The resource (`interface.resource`).
        resource: String,
    },
    /// A scheduler error: an invalid config, an unknown node, or a graph
    /// the scheduler cannot run.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Runtime(#[from] RuntimeError),
}

/// What resolves an ambiguous capability: renaming its label after one
/// provider, labelling it so (an interface imported by its id), or else
/// keeping only one of the providers.
fn ambiguity_fix(capability: &str, labelled: bool) -> &'static str {
    // Only an interface id has a `/`.
    let by_id = capability.contains('/');
    if labelled {
        "rename the label after one of them"
    } else if by_id {
        "import it under a label naming one of them"
    } else {
        "WIT cannot label it, so register only one of them"
    }
}
