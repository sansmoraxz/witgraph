//! Port definitions and semantics.

use crate::Type;
use crate::id::PortName;

/// The delivery semantics of a port, derived from the WIT field type.
///
/// Compatibility is exact-kind-match only: no coercion between kinds, no
/// numeric widening, no option-lifting, no record width subtyping.
///
/// Displays as the lowercase kind name (`value`, `stream`, `future`,
/// `handle`) for diagnostics.
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
    /// A handle to a resource the host provides ([`HandleType`]): borrowed
    /// by an input for the length of `run`, owned by an output and handed
    /// to the host when `run` returns. Delivered like a Value, and equal
    /// only to itself: the host never looks inside.
    Handle,
}

impl PortKind {
    /// Whether ports of these kinds may be connected: exact match only.
    pub fn compatible(self, other: PortKind) -> bool {
        self == other
    }

    /// Whether the host latches what the kind carries: a Value or a handle,
    /// delivered whole when `run` starts and taken whole when it returns,
    /// unlike a stream or future, which lives inside one run.
    pub fn is_latched(self) -> bool {
        matches!(self, PortKind::Value | PortKind::Handle)
    }

    /// Whether the kind is carried by a component-model async handle
    /// (`stream` or `future`).
    pub fn is_async(self) -> bool {
        matches!(self, PortKind::Stream | PortKind::Future)
    }
}

/// The resource a [`Handle`](PortKind::Handle) port carries: one declared
/// in a named interface the component imports, so every node importing
/// that interface names the same resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HandleType {
    /// The declaring interface's full id
    /// (`namespace:package/interface@version`).
    pub interface: String,
    /// The resource's WIT name.
    pub resource: String,
}

impl HandleType {
    /// Whether a port carrying `self` may feed one carrying `other`: the
    /// same resource of the same interface, at semver-compatible versions.
    pub fn connects(&self, other: &HandleType) -> bool {
        self.resource == other.resource
            && crate::interface::semver_compatible(&self.interface, &other.interface)
    }
}

impl std::fmt::Display for HandleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.interface, self.resource)
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
    /// The payload type carried by the port. `None` for a bare `stream` or
    /// `future`, which carries no payload, and for a handle.
    pub ty: Option<Type>,
    /// Handle ports only: the resource the port carries.
    pub handle: Option<HandleType>,
    /// Value input ports only: the port may be left unconnected, in which
    /// case it reads as absent (`none`) until a runtime writes a value.
    pub optional: bool,
    /// Doc comment from the WIT field, if any.
    pub docs: Option<String>,
    /// Output ports only: how an editor should preview the port's values
    /// (`image`, say), as the component's `witgraph-viewer` section
    /// declares it. Opaque here; WIT text never sets it. Like `docs`, it is
    /// part of equality, so contracts compared with each other must come
    /// from the same source.
    pub viewer: Option<String>,
}

impl PortDef {
    /// A required, undocumented port carrying `ty`.
    pub fn new(name: impl Into<PortName>, kind: PortKind, ty: Type) -> Self {
        Self {
            name: name.into(),
            kind,
            ty: Some(ty),
            handle: None,
            optional: false,
            docs: None,
            viewer: None,
        }
    }

    /// A required, undocumented port with no payload (a bare `stream` or
    /// `future`).
    pub fn unit(name: impl Into<PortName>, kind: PortKind) -> Self {
        Self {
            name: name.into(),
            kind,
            ty: None,
            handle: None,
            optional: false,
            docs: None,
            viewer: None,
        }
    }

    /// A required, undocumented handle port carrying `handle`.
    pub fn handle(name: impl Into<PortName>, handle: HandleType) -> Self {
        Self {
            name: name.into(),
            kind: PortKind::Handle,
            ty: None,
            handle: Some(handle),
            optional: false,
            docs: None,
            viewer: None,
        }
    }

    /// WIT-syntax rendering of the payload type, or the handle's resource
    /// (`interface.resource`); `_` when there is none.
    pub fn type_display(&self) -> String {
        match (&self.ty, &self.handle) {
            (Some(ty), _) => ty.to_string(),
            (None, Some(handle)) => handle.to_string(),
            (None, None) => "_".into(),
        }
    }

    /// Whether what this output carries may feed `input`, kinds aside:
    /// equal payload types, an option unwrapping into an optional input
    /// ([`unwraps_into`](Self::unwraps_into)), or connecting handles.
    pub fn carries_into(&self, input: &PortDef) -> bool {
        match (&self.handle, &input.handle) {
            (Some(from), Some(to)) => from.connects(to),
            (None, None) => self.ty == input.ty || self.unwraps_into(input),
            _ => false,
        }
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
        let kinds = [
            PortKind::Value,
            PortKind::Stream,
            PortKind::Future,
            PortKind::Handle,
        ];
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
        assert_eq!(PortKind::Handle.to_string(), "handle");
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
        assert!(!PortKind::Handle.is_async());
        for kind in [
            PortKind::Value,
            PortKind::Stream,
            PortKind::Future,
            PortKind::Handle,
        ] {
            assert_eq!(kind.is_latched(), !kind.is_async());
        }
    }

    fn tensor(interface: &str) -> HandleType {
        HandleType {
            interface: interface.into(),
            resource: "tensor".into(),
        }
    }

    #[test]
    fn handles_connect_by_resource_and_compatible_interface() {
        let a = tensor("test:data/tensor@0.1.0");
        assert!(a.connects(&tensor("test:data/tensor@0.1.3")));
        assert!(!a.connects(&tensor("test:data/tensor@0.2.0")));
        assert!(!a.connects(&tensor("test:other/tensor@0.1.0")));
        let blob = HandleType {
            resource: "blob".into(),
            ..a.clone()
        };
        assert!(!a.connects(&blob));

        let out = PortDef::handle("t", a.clone());
        assert!(out.carries_into(&PortDef::handle("t", tensor("test:data/tensor@0.1.1"))));
        assert!(!out.carries_into(&PortDef::handle("t", blob)));
        assert!(!out.carries_into(&PortDef::new("t", PortKind::Value, Type::U8)));
        assert_eq!(out.type_display(), "test:data/tensor@0.1.0.tensor");
    }
}
