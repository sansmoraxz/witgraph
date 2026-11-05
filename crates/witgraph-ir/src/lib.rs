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
pub mod index;
pub mod interface;
pub mod partition;
pub mod port;
mod topo;
pub mod wave;

pub use compile::{
    CompilationFailure, CompiledConnection, CompiledGraph, CompiledNode, ContractIndex,
    ContractSource, LinkKey, RequiredCapability, Resolution, resolve_contract,
};
pub use component::{Capability, ComponentContract};
pub use diagnostics::{Diagnostic, Diagnostics, Location, Severity};
pub use graph::{
    Amount, Capacity, Connection, Graph, GraphBuilder, GraphMetadata, Link, Node, ResourceClaim,
    ResourceDecl, UnknownNodeError,
};
pub use id::{
    ComponentRef, ConnectionId, LinkId, NodeId, PackageRef, ParseComponentRefError, PortName,
    PortRef, ResourceId,
};
pub use index::{
    FeedbackEntry, FeedbackIdx, Idx, IndexVec, InputIdx, NodeIdx, OutputIdx, PortEntry, PortSet,
    PortTable,
};
pub use port::{Carried, HandleType, NodeShape, PortDef, PortDirection, PortKind, PortType};
pub use wasm_wave;
pub use wasm_wave::value::Type;
