//! Component contracts and capability requirements.

use std::collections::BTreeMap;

use crate::id::ComponentRef;
use crate::port::{NodeShape, PortDef};

/// A capability a component requires from its host.
///
/// A component's WIT world's imported functions, its imported interfaces
/// that carry functions or declare resources, and the resources it declares
/// itself are its capabilities: each is "something the host must provide".
/// Type-only imports (bare types, interfaces with neither) are structural,
/// not capabilities, and neither is the built-in `witgraph:runtime/host`
/// interface.
///
/// A capability is named after what the component imports:
/// - a named interface by its full id (`wasi:clocks/monotonic-clock@0.2.3`);
/// - an anonymous inline interface by its import name (`config`);
/// - a bare function by its import name prefixed `func:` (`func:blink`;
///   the host links `blink` at its linker's root);
/// - a resource declared in the world by its name prefixed `resource:`
///   (`resource:r`; the host links `r`, its constructor, methods and static
///   functions at the root), with those functions as its items.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Capability {
    /// Full interface id, inline interface import name, `func:`-prefixed
    /// bare function import name, or `resource:`-prefixed world resource
    /// name.
    pub interface: String,
    /// What the host implements for it: each function (`[method]`,
    /// `[static]` and `[constructor]` ones included) and resource, by name,
    /// mapped to a canonical rendering of its signature (`resource` for a
    /// resource). A bare function import has one item, under its own name.
    /// Part of the content hash, so two revisions whose capabilities differ
    /// only in signature hash differently.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub items: BTreeMap<String, String>,
}

impl Capability {
    /// A capability on the given interface id, with no items recorded.
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            items: BTreeMap::new(),
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
