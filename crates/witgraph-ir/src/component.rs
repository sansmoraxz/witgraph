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
///   functions at the root), with those functions as its items;
/// - a named interface imported under a label (`import primary: clock;`) by
///   that label, with the interface's full id in
///   [`implements`](Self::implements). A world may import one interface
///   under several labels, each a capability of its own, so a host can back
///   each with a different implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Capability {
    /// Full interface id, inline interface import name, `func:`-prefixed
    /// bare function import name, `resource:`-prefixed world resource
    /// name, or the label of a labelled interface import. The host links
    /// the capability under [`link_name`](Self::link_name).
    pub interface: String,
    /// For a labelled import, the full id of the interface the label
    /// stands for (`namespace:name/interface@version`); `None` otherwise.
    /// Part of the content hash.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub implements: Option<String>,
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
    /// Whether the capability is an interface the component imports (by
    /// its id, an inline import name or a label), rather than a bare
    /// function (`func:`) or a world resource (`resource:`). Only an
    /// interface can be satisfied by a [`Link`](crate::Link).
    pub fn is_interface(&self) -> bool {
        self.bare_name().is_none()
    }

    /// The name a host links the capability under: an interface's id,
    /// inline import name or label; a bare function's or world resource's
    /// own name, at the linker's root (`blink` for `func:blink`).
    pub fn link_name(&self) -> &str {
        self.bare_name().unwrap_or(&self.interface)
    }

    /// A bare function's or world resource's name, without its prefix. An
    /// interface id always has a `/` (even one from a package namespaced
    /// `func` or `resource`), and the prefixed names never do.
    fn bare_name(&self) -> Option<&str> {
        if self.interface.contains('/') {
            return None;
        }
        self.interface
            .strip_prefix("func:")
            .or_else(|| self.interface.strip_prefix("resource:"))
    }

    /// A capability on the given interface id, with no items recorded.
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            implements: None,
            items: BTreeMap::new(),
        }
    }
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

    #[test]
    fn prefixed_names_are_bare_and_interface_ids_are_not() {
        for (name, interface, link) in [
            ("func:blink", false, "blink"),
            ("resource:r", false, "r"),
            ("config", true, "config"),
            ("demo:caps/clock@0.1.0", true, "demo:caps/clock@0.1.0"),
            // A package may be namespaced `resource` or `func`.
            (
                "resource:pool/alloc@1.0.0",
                true,
                "resource:pool/alloc@1.0.0",
            ),
            ("func:tools/run", true, "func:tools/run"),
        ] {
            let capability = Capability::new(name);
            assert_eq!(capability.is_interface(), interface, "{name}");
            assert_eq!(capability.link_name(), link, "{name}");
        }
    }
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
}
