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
pub mod node;
pub mod partition;
pub mod port;
mod topo;
pub mod wave;

pub use compile::{
    CompilationFailure, CompiledConnection, CompiledGraph, CompiledNode, ContractSource, LinkKey,
    RequiredCapability,
};
pub use component::{Capability, ComponentContract};
pub use diagnostics::{Diagnostic, Diagnostics, Location, Severity};
pub use graph::{
    Amount, Capacity, ComponentEntry, Connection, Graph, GraphBuilder, GraphMetadata, Link, Node,
    ResourceClaim, ResourceDecl, UnknownNodeError,
};
pub use id::{
    ComponentName, ConnectionId, ContractId, LinkId, NodeId, ParseRefError, PortName, PortRef,
    ResourceId, WitRef, WorldRef,
};
pub use index::{
    FeedbackEntry, FeedbackIdx, Idx, IndexVec, InputIdx, NodeIdx, OutputIdx, PortEntry, PortSet,
    PortTable,
};
pub use node::NodeDescription;
pub use port::{Carried, HandleType, NodeShape, PortDef, PortDirection, PortKind, PortType};
pub use wasm_wave;
pub use wasm_wave::value::Type;
