//! Port definitions and semantics.

use crate::Type;
use crate::id::PortName;

/// The delivery semantics of a port, derived from the WIT field type.
///
/// Compatibility is exact-kind-match only: no coercion between kinds, no
/// numeric widening, no option-lifting, no record width subtyping.
///
/// Displays as the lowercase kind name (`value`, `stream`, `future`) for
/// diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum PortKind {
    /// Passed by value: an input is read when the node's `run` starts, an
    /// output is latched when `run` returns.
    Value,
    /// A WIT `stream<T>`: ordered, back-pressured sequence with end-of-stream.
    Stream,
    /// A WIT `future<T>`: exactly one resolution.
    Future,
}

impl PortKind {
    /// Whether ports of these kinds may be connected: exact match only.
    pub fn compatible(self, other: PortKind) -> bool {
        self == other
    }

    /// Whether the kind is carried by a component-model async handle
    /// (`stream` or `future`).
    pub fn is_async(self) -> bool {
        matches!(self, PortKind::Stream | PortKind::Future)
    }
}

/// How a node participates in execution, derived from its ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum NodeShape {
    /// Every port is a Value: the node re-runs whenever an input value
    /// changes.
    Reactive,
    /// At least one input or output is a Stream or Future: the node runs
    /// once per generation and its streams are its live channel.
    Streaming,
}

/// Which side of a component a port sits on.
///
/// Displays as the lowercase direction name (`input`, `output`) for
/// diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum PortDirection {
    /// The component consumes through this port.
    Input,
    /// The component produces through this port.
    Output,
}

/// One typed port on a component contract.
///
/// Contracts are always re-derived from WIT, so ports are not serializable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDef {
    /// The port's WIT kebab-case name.
    pub name: PortName,
    /// The port's delivery semantics.
    pub kind: PortKind,
    /// The payload type carried by the port. `None` only for a bare
    /// `stream` or `future`, which carries no payload.
    pub ty: Option<Type>,
    /// Value input ports only: the port may be left unconnected, in which
    /// case it reads as absent (`none`) until a runtime writes a value.
    pub optional: bool,
    /// Doc comment from the WIT field, if any.
    pub docs: Option<String>,
}

impl PortDef {
    /// A required, undocumented port carrying `ty`.
    pub fn new(name: impl Into<PortName>, kind: PortKind, ty: Type) -> Self {
        Self {
            name: name.into(),
            kind,
            ty: Some(ty),
            optional: false,
            docs: None,
        }
    }

    /// A required, undocumented port with no payload (a bare `stream` or
    /// `future`).
    pub fn unit(name: impl Into<PortName>, kind: PortKind) -> Self {
        Self {
            name: name.into(),
            kind,
            ty: None,
            optional: false,
            docs: None,
        }
    }

    /// WIT-syntax rendering of the payload type; `_` when there is none.
    pub fn type_display(&self) -> String {
        self.ty
            .as_ref()
            .map_or_else(|| "_".into(), ToString::to_string)
    }

    /// Marks the port as safe to leave unconnected (Value inputs only).
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Whether this Value output, of type `option<T>`, feeds `input`, an
    /// optional Value input of payload `T`: the option passes straight
    /// through (`none` reads as `none`, `some(x)` as `x`). Equal types are
    /// the ordinary case and do not count.
    pub fn unwraps_into(&self, input: &PortDef) -> bool {
        self.kind == PortKind::Value
            && input.kind == PortKind::Value
            && input.optional
            && self.ty != input.ty
            && self.ty.is_some()
            && self.ty == input.ty.clone().map(Type::option)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_only_match_themselves() {
        let kinds = [PortKind::Value, PortKind::Stream, PortKind::Future];
        for a in kinds {
            for b in kinds {
                assert_eq!(a.compatible(b), a == b);
            }
        }
    }

    /// The content-hash canonical encoding embeds these renderings; changing
    /// a spelling changes every contract hash.
    #[test]
    fn display_spellings_are_pinned() {
        assert_eq!(PortKind::Value.to_string(), "value");
        assert_eq!(PortKind::Stream.to_string(), "stream");
        assert_eq!(PortKind::Future.to_string(), "future");
        assert_eq!(PortDirection::Input.to_string(), "input");
        assert_eq!(PortDirection::Output.to_string(), "output");
    }

    #[test]
    fn payload_display() {
        assert_eq!(
            PortDef::new("a", PortKind::Value, Type::F64).type_display(),
            "f64"
        );
        assert_eq!(PortDef::unit("t", PortKind::Stream).type_display(), "_");
    }

    #[test]
    fn async_kinds() {
        assert!(!PortKind::Value.is_async());
        assert!(PortKind::Stream.is_async());
        assert!(PortKind::Future.is_async());
    }
}
