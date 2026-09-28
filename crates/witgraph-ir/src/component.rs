//! Component contracts and capability requirements.

use crate::id::ComponentRef;
use crate::port::{ConsumptionMode, PortDef};
use crate::types::Type;

/// A capability a component requires from its host.
///
/// A component's WIT world's imported functions and function-carrying
/// interfaces are its capabilities: each is "something the host must
/// provide". Type-only imports (bare types, function-less interfaces) are
/// structural, not capabilities. Named interface imports render in full id
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

/// A named type declared in a component's WIT, kept for diagnostics and
/// metadata rendering. Names are not part of [`Type`] — equality stays
/// structural.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TypeDecl {
    /// The WIT-declared type name.
    pub name: String,
    /// The structural type the name resolves to.
    pub ty: Type,
    /// Doc comment from the WIT declaration, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub docs: Option<String>,
}

/// The public contract of a WIT-defined component.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComponentContract {
    /// The component's version identity.
    pub id: ComponentRef,
    /// The component's input ports.
    pub inputs: Vec<PortDef>,
    /// The component's output ports.
    pub outputs: Vec<PortDef>,
    /// What the component requires from its host (its world imports).
    pub capabilities: Vec<Capability>,
    /// Named WIT types, for diagnostics and metadata rendering.
    pub type_names: Vec<TypeDecl>,
    /// Doc comment from the WIT world, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub docs: Option<String>,
}

impl ComponentContract {
    /// The node's effective consumption mode.
    ///
    /// Derived from the input ports: [`Async`](ConsumptionMode::Async) iff
    /// any undrained input is `Stream`/`Event`/`Future`, otherwise
    /// [`Sync`](ConsumptionMode::Sync) (a node with no inputs is a total
    /// function over zero inputs). Drained inputs complete before the node's
    /// first activation and are delivered as latched totals, so they never
    /// color the node async.
    ///
    /// The `drained` flags are taken at face value: on an uncompiled contract
    /// a drained Value or Event input (rejected at compilation as
    /// undrainable) is still treated as non-coloring here.
    pub fn consumption_mode(&self) -> ConsumptionMode {
        if self
            .inputs
            .iter()
            .any(|input| !input.drained && input.kind.consumption_mode() == ConsumptionMode::Async)
        {
            ConsumptionMode::Async
        } else {
            ConsumptionMode::Sync
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::PackageRef;
    use crate::port::PortKind;

    fn contract(inputs: Vec<PortDef>) -> ComponentContract {
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
            outputs: vec![],
            capabilities: vec![],
            type_names: vec![],
            docs: None,
        }
    }

    #[test]
    fn consumption_mode_from_inputs() {
        use crate::types::Type;

        let sync = contract(vec![
            PortDef::new("a", PortKind::Value, Type::F64),
            PortDef::new("b", PortKind::Value, Type::U32),
        ]);
        assert_eq!(sync.consumption_mode(), ConsumptionMode::Sync);

        let r#async = contract(vec![
            PortDef::new("a", PortKind::Stream, Type::F64),
            PortDef::new("b", PortKind::Event, Type::U32),
        ]);
        assert_eq!(r#async.consumption_mode(), ConsumptionMode::Async);

        let mixed = contract(vec![
            PortDef::new("a", PortKind::Value, Type::F64),
            PortDef::new("b", PortKind::Stream, Type::U32),
        ]);
        assert_eq!(
            mixed.consumption_mode(),
            ConsumptionMode::Async,
            "any async input colors the node async; values are latched params"
        );

        let no_inputs = contract(vec![]);
        assert_eq!(no_inputs.consumption_mode(), ConsumptionMode::Sync);
    }

    #[test]
    fn drained_inputs_never_color_async() {
        use crate::types::Type;

        let all_drained = contract(vec![
            PortDef::new("a", PortKind::Stream, Type::F64).drained(),
            PortDef::new("b", PortKind::Future, Type::U32).drained(),
        ]);
        assert_eq!(all_drained.consumption_mode(), ConsumptionMode::Sync);

        let mixed = contract(vec![
            PortDef::new("a", PortKind::Stream, Type::F64).drained(),
            PortDef::new("b", PortKind::Stream, Type::U32),
        ]);
        assert_eq!(
            mixed.consumption_mode(),
            ConsumptionMode::Async,
            "an undrained async input still colors the node async"
        );
    }
}
