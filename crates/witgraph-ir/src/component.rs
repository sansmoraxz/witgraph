//! Component contracts and capability requirements.

use crate::id::ComponentRef;
use crate::port::{NodeShape, PortDef};

/// A capability a component requires from its host.
///
/// A component's WIT world's imported functions and function-carrying
/// interfaces are its capabilities: each is "something the host must
/// provide". Type-only imports (bare types, function-less interfaces) are
/// structural, not capabilities, and neither are imports from the built-in
/// `witgraph:runtime` package. Named interface imports render in full id
/// form (`wasi:clocks/monotonic-clock@0.2.3`); anonymous inline interface
/// imports render world-scoped (`demo:graph/world.import-name@0.1.0`); bare
/// function imports are prefixed `func:`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Capability {
    /// Full interface id, or a `func:`-prefixed bare function import name.
    pub interface: String,
}

impl Capability {
    /// A capability on the given interface id.
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
        }
    }
}

/// How the component's `run` export is declared.
///
/// Displays as `sync` / `async`; the content hash embeds this rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum RunKind {
    /// `run: func(...)`.
    Sync,
    /// `run: async func(...)`.
    Async,
}

/// The public contract of a WIT-defined component.
///
/// Never serialized: a graph stores only [`ComponentRef`]s, and contracts are
/// re-derived from WIT (see [`ContractSource`](crate::ContractSource)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContract {
    /// The component's version identity.
    pub id: ComponentRef,
    /// The component's input ports (fields of its `inputs` record).
    pub inputs: Vec<PortDef>,
    /// The component's output ports (fields of its `outputs` record).
    pub outputs: Vec<PortDef>,
    /// Whether `run` is a plain or an `async` function.
    pub run: RunKind,
    /// What the component requires from its host (its world imports).
    pub capabilities: Vec<Capability>,
    /// Doc comment from the WIT world, if any.
    pub docs: Option<String>,
}

impl From<&ComponentContract> for ComponentRef {
    fn from(contract: &ComponentContract) -> Self {
        contract.id.clone()
    }
}

impl From<ComponentContract> for ComponentRef {
    fn from(contract: ComponentContract) -> Self {
        contract.id
    }
}

impl ComponentContract {
    /// The node's execution shape: [`Streaming`](NodeShape::Streaming) iff
    /// any input or output port is a Stream or Future, otherwise
    /// [`Reactive`](NodeShape::Reactive).
    pub fn shape(&self) -> NodeShape {
        if self
            .inputs
            .iter()
            .chain(&self.outputs)
            .any(|port| port.kind.is_async())
        {
            NodeShape::Streaming
        } else {
            NodeShape::Reactive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Type;
    use crate::id::PackageRef;
    use crate::port::PortKind;

    fn contract(inputs: Vec<PortDef>, outputs: Vec<PortDef>) -> ComponentContract {
        ComponentContract {
            id: ComponentRef {
                package: PackageRef {
                    namespace: "demo".into(),
                    name: "graph".into(),
                    version: None,
                },
                world: "test".into(),
                content_hash: None,
            },
            inputs,
            outputs,
            run: RunKind::Sync,
            capabilities: vec![],
            docs: None,
        }
    }

    #[test]
    fn shape_from_ports() {
        let reactive = contract(
            vec![PortDef::new("a", PortKind::Value, Type::F64)],
            vec![PortDef::new("b", PortKind::Value, Type::U32)],
        );
        assert_eq!(reactive.shape(), NodeShape::Reactive);

        let streaming_in = contract(
            vec![
                PortDef::new("a", PortKind::Value, Type::F64),
                PortDef::new("b", PortKind::Stream, Type::U32),
            ],
            vec![],
        );
        assert_eq!(streaming_in.shape(), NodeShape::Streaming);

        let streaming_out = contract(vec![], vec![PortDef::new("f", PortKind::Future, Type::U32)]);
        assert_eq!(
            streaming_out.shape(),
            NodeShape::Streaming,
            "an async output alone makes the node streaming"
        );

        assert_eq!(contract(vec![], vec![]).shape(), NodeShape::Reactive);
    }

    #[test]
    fn run_kind_spellings_are_pinned() {
        assert_eq!(RunKind::Sync.to_string(), "sync");
        assert_eq!(RunKind::Async.to_string(), "async");
    }
}
