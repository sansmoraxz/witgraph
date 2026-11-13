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

/// What a stream or future carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carried {
    /// Nothing: a bare `stream` or `future`, which only signals.
    Nothing,
    /// Data of this type.
    Data(Type),
    /// An owned handle, which moves to whoever reads it.
    Handle(HandleType),
}

/// What a port carries, and how it is delivered. What a port can be is exactly what
/// this can say: a Value always has a type, and only a Value or a handle can be left
/// unconnected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortType {
    /// A value, latched.
    Value {
        /// The payload type.
        ty: Type,
        /// Input ports only: the port may be left unconnected, in which case it
        /// reads as absent (`none`) until a runtime writes a value.
        optional: bool,
    },
    /// A resource handle, latched.
    Handle {
        /// The resource.
        handle: HandleType,
        /// Input ports only: as for a value.
        optional: bool,
    },
    /// A stream, read once.
    Stream(Carried),
    /// A future, read once.
    Future(Carried),
}

/// One typed port on a component contract.
///
/// Contracts are always re-derived from WIT, so ports are not serializable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDef {
    /// The port's WIT kebab-case name.
    pub name: PortName,
    /// What it carries, and how.
    pub port_type: PortType,
    /// Doc comment from the WIT field, if any.
    pub docs: Option<String>,
    /// Output ports only: how an editor should preview the port's values
    /// (`image`, say), as the component's node manifest declares it. Opaque here; WIT text never sets it. Like `docs`, it is
    /// part of equality, so contracts compared with each other must come
    /// from the same source.
    pub viewer: Option<String>,
}

impl PortDef {
    /// An undocumented port of `port_type`.
    pub fn of(name: impl Into<PortName>, port_type: PortType) -> Self {
        Self {
            name: name.into(),
            port_type,
            docs: None,
            viewer: None,
        }
    }

    /// A required, undocumented value port.
    pub fn value(name: impl Into<PortName>, ty: Type) -> Self {
        Self::of(
            name,
            PortType::Value {
                ty,
                optional: false,
            },
        )
    }

    /// A required, undocumented handle port.
    pub fn handle(name: impl Into<PortName>, handle: HandleType) -> Self {
        Self::of(
            name,
            PortType::Handle {
                handle,
                optional: false,
            },
        )
    }

    /// An undocumented stream port.
    pub fn stream(name: impl Into<PortName>, carried: Carried) -> Self {
        Self::of(name, PortType::Stream(carried))
    }

    /// An undocumented future port.
    pub fn future(name: impl Into<PortName>, carried: Carried) -> Self {
        Self::of(name, PortType::Future(carried))
    }

    /// The port's delivery semantics.
    pub fn kind(&self) -> PortKind {
        match self.port_type {
            PortType::Value { .. } => PortKind::Value,
            PortType::Handle { .. } => PortKind::Handle,
            PortType::Stream(_) => PortKind::Stream,
            PortType::Future(_) => PortKind::Future,
        }
    }

    /// The payload type the port carries, if it carries data.
    pub fn ty(&self) -> Option<&Type> {
        match &self.port_type {
            PortType::Value { ty, .. }
            | PortType::Stream(Carried::Data(ty))
            | PortType::Future(Carried::Data(ty)) => Some(ty),
            _ => None,
        }
    }

    /// The resource the port carries, if it carries one.
    pub fn handle_type(&self) -> Option<&HandleType> {
        match &self.port_type {
            PortType::Handle { handle, .. }
            | PortType::Stream(Carried::Handle(handle))
            | PortType::Future(Carried::Handle(handle)) => Some(handle),
            _ => None,
        }
    }

    /// Whether the port may be left unconnected.
    pub fn is_optional(&self) -> bool {
        matches!(
            self.port_type,
            PortType::Value { optional: true, .. } | PortType::Handle { optional: true, .. }
        )
    }

    /// WIT-syntax rendering of the payload type, or the handle's resource
    /// (`interface.resource`); `_` when there is none.
    pub fn type_display(&self) -> String {
        match (self.ty(), self.handle_type()) {
            (Some(ty), _) => ty.to_string(),
            (None, Some(handle)) => handle.to_string(),
            (None, None) => "_".into(),
        }
    }

    /// Whether what this output carries may feed `input`, kinds aside:
    /// equal payload types, an option unwrapping into an optional input
    /// ([`unwraps_into`](Self::unwraps_into)), or connecting handles.
    pub fn carries_into(&self, input: &PortDef) -> bool {
        match (self.handle_type(), input.handle_type()) {
            (Some(from), Some(to)) => from.connects(to),
            (None, None) => self.ty() == input.ty() || self.unwraps_into(input),
            _ => false,
        }
    }

    /// Marks a value or handle port as safe to leave unconnected. A stream or
    /// future cannot be, so it stays as it is.
    pub fn optional(mut self) -> Self {
        if let PortType::Value { optional, .. } | PortType::Handle { optional, .. } =
            &mut self.port_type
        {
            *optional = true;
        }
        self
    }

    /// Whether this Value output, of type `option<T>`, feeds `input`, an
    /// optional Value input of payload `T`: the option passes straight
    /// through (`none` reads as `none`, `some(x)` as `x`). Equal types are
    /// the ordinary case and do not count.
    pub fn unwraps_into(&self, input: &PortDef) -> bool {
        let (
            PortType::Value { ty: from, .. },
            PortType::Value {
                ty: to,
                optional: true,
            },
        ) = (&self.port_type, &input.port_type)
        else {
            return false;
        };
        from != to && *from == Type::option(to.clone())
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
        assert_eq!(PortDef::value("a", Type::F64).type_display(), "f64");
        assert_eq!(
            PortDef::stream("t", crate::port::Carried::Nothing).type_display(),
            "_"
        );
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
        assert!(!out.carries_into(&PortDef::value("t", Type::U8)));
        assert_eq!(out.type_display(), "test:data/tensor@0.1.0.tensor");
    }
}
