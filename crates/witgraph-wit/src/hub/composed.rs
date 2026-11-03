//! Making a graph one component.

use std::collections::HashMap;

use wac_graph::types::Package;
use wac_graph::{CompositionGraph, EncodeOptions, NodeId as WacNode};
use witgraph_ir::{CompiledGraph, NodeId};

use super::{CONTROL_INTERFACE, PORTS_INTERFACE, generate, host_import};
use crate::lower::HOST_INTERFACE;

/// One node of the graph, with the component it runs.
#[derive(Debug, Clone, Copy)]
pub struct HubMember<'a> {
    /// The node.
    pub node: &'a NodeId,
    /// Its component, with its links composed in.
    pub bytes: &'a [u8],
    /// The name its `node` interface is exported under.
    pub export: &'a str,
}

/// A graph as one component.
#[derive(Debug, Clone)]
pub struct ComposedGraph {
    /// The composed component: the hub and every node.
    pub bytes: Vec<u8>,
    /// How many component instances it holds.
    pub components: usize,
}

/// Why a graph could not be made into one component.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    /// A node's component could not be read.
    #[error("`{node}`: {message}")]
    Member {
        /// The node.
        node: NodeId,
        /// What is wrong.
        message: String,
    },
    /// The graph has a node nothing was given for.
    #[error("no component for node `{0}`")]
    MissingMember(NodeId),
    /// A port of a kind the hub cannot carry.
    #[error("not supported in a hub: {0}")]
    Unsupported(String),
    /// Generating the hub failed: a bug here, or a component it did not
    /// expect.
    #[error("generating the hub: {0}")]
    Generate(String),
    /// Composing the hub with the nodes failed.
    #[error("composing the graph: {0}")]
    Compose(String),
}

/// Makes `compiled` one component: a generated hub, and an instance of each
/// member's component, which share one package per distinct bytes.
pub fn compose_graph(
    compiled: &CompiledGraph,
    members: &[HubMember<'_>],
) -> Result<ComposedGraph, HubError> {
    let generated = generate::hub(compiled, members)?;
    let compose = |e: String| HubError::Compose(e);
    let mut graph = CompositionGraph::new();
    let hub = Package::from_bytes("witgraph:hub", None, generated.bytes, graph.types_mut())
        .map_err(|e| compose(format!("{e:#}")))?;
    let hub = graph
        .register_package(hub)
        .map_err(|e| compose(format!("{e:#}")))?;
    let hub = graph.instantiate(hub);

    let mut packages: HashMap<&[u8], wac_graph::PackageId> = HashMap::new();
    let mut components = 1;
    for (position, member) in generated.order.iter().enumerate() {
        let package = match packages.get(member.bytes) {
            Some(package) => *package,
            None => {
                let name = format!("witgraph:node-{}", packages.len());
                let package = Package::from_bytes(&name, None, member.bytes, graph.types_mut())
                    .map_err(|e| compose(format!("`{}`: {e:#}", member.node)))?;
                let package = graph
                    .register_package(package)
                    .map_err(|e| compose(format!("`{}`: {e:#}", member.node)))?;
                packages.insert(member.bytes, package);
                package
            }
        };
        let instance: WacNode = graph.instantiate(package);
        components += 1;
        // `fatal` tells who called it by which import it came through, and
        // every other import of one name merges into one.
        let world = graph[package].ty();
        let host: Vec<(String, wac_graph::types::ItemKind)> = graph.types()[world]
            .imports
            .iter()
            .filter(|(name, _)| name.starts_with(HOST_INTERFACE))
            .map(|(name, kind)| (name.clone(), *kind))
            .collect();
        for (name, kind) in host {
            let import = graph
                .import(host_import(position), kind.promote())
                .map_err(|e| compose(format!("`{}`: {e:#}", member.node)))?;
            graph
                .set_instantiation_argument(instance, &name, import)
                .map_err(|e| compose(format!("`{}`: {e:#}", member.node)))?;
        }
        let export = graph
            .alias_instance_export(instance, member.export)
            .map_err(|e| compose(format!("`{}`: {e:#}", member.node)))?;
        graph
            .set_instantiation_argument(hub, &format!("m{position}"), export)
            .map_err(|e| compose(format!("`{}` as an import of the hub: {e:#}", member.node)))?;
    }
    for interface in [CONTROL_INTERFACE, PORTS_INTERFACE] {
        let export = graph
            .alias_instance_export(hub, interface)
            .map_err(|e| compose(format!("{e:#}")))?;
        graph
            .export(export, interface)
            .map_err(|e| compose(format!("{e:#}")))?;
    }
    let bytes = graph
        .encode(EncodeOptions {
            validate: true,
            ..EncodeOptions::default()
        })
        .map_err(|e| compose(format!("{e:#}")))?;
    Ok(ComposedGraph { bytes, components })
}
