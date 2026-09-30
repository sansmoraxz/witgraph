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
pub mod types;
pub mod val;

pub use compile::{CompilationFailure, CompiledGraph};
pub use component::{Capability, ComponentContract, TypeDecl};
pub use diagnostics::{Diagnostic, Diagnostics, Location, Severity};
pub use graph::{Connection, Fraction, Graph, GraphBuilder, GraphMetadata, Node, ResourceClaim};
pub use id::{
    ComponentRef, ConnectionId, NodeId, PackageRef, ParseComponentRefError, PortName, PortRef,
    ResourceId,
};
pub use port::{ConsumptionMode, PortDef, PortDirection, PortKind};
pub use types::{Case, EnumType, Field, FlagsType, Record, Type, Variant};
pub use val::Val;
