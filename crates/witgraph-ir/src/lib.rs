//! Typed graph IR and component model for witgraph.
//!
//! Pipeline typestate chain: [`GraphBuilder`] → [`Graph`] → [`CompiledGraph`].
//! [`Graph`] is plain serializable data — openly constructible, field by
//! field or by deserialization — so the chain's guarantee sits entirely on
//! [`CompiledGraph`]: it is sealed (no public constructor, not
//! deserializable), and the only way in is [`Graph::compile`]. APIs that
//! need a validated graph take [`CompiledGraph`].

pub mod compile;
pub mod component;
pub mod diagnostics;
pub mod graph;
pub mod id;
pub mod port;
mod topo;

pub use compile::{
    CompilationFailure, CompiledConnection, CompiledGraph, CompiledNode, ContractIndex,
    ContractSource, RequiredCapability, Resolution, resolve_contract,
};
pub use component::{Capability, ComponentContract, RunKind};
pub use diagnostics::{Diagnostic, Diagnostics, Location, Severity};
pub use graph::{
    Connection, Fraction, Graph, GraphBuilder, GraphMetadata, Node, ResourceClaim, UnknownNodeError,
};
pub use id::{
    ComponentRef, ConnectionId, NodeId, PackageRef, ParseComponentRefError, PortName, PortRef,
    ResourceId,
};
pub use port::{NodeShape, PortDef, PortDirection, PortKind};
pub use wasm_wave;
pub use wasm_wave::value::Type;
