//! Port definitions and semantics.

use crate::id::PortName;
use crate::types::Type;

/// The delivery semantics of a port.
///
/// Compatibility is exact-kind-match only: no coercion between kinds
/// (Event never connects to Stream and vice versa), no numeric widening,
/// no option-lifting, no record width subtyping.
///
/// Displays as the lowercase kind name (`value`, `event`, `stream`,
/// `future`) for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum PortKind {
    /// Latched, continuous: always readable, last-write-wins.
    Value,
    /// Discrete occurrences; the consumer observes each occurrence once.
    Event,
    /// Ordered, back-pressured sequence with end-of-stream.
    Stream,
    /// Exactly one resolution.
    Future,
}

impl PortKind {
    /// Whether ports of these kinds may be connected: exact match only.
    pub fn compatible(self, other: PortKind) -> bool {
        self == other
    }

    /// [`Sync`](ConsumptionMode::Sync) for `Value`; async for the rest.
    pub fn consumption_mode(self) -> ConsumptionMode {
        match self {
            PortKind::Value => ConsumptionMode::Sync,
            PortKind::Event | PortKind::Stream | PortKind::Future => ConsumptionMode::Async,
        }
    }
}

/// How a node consumes its inputs.
///
/// Derived per node from its input ports: any undrained Stream/Event/Future
/// input colors the node async, and its Value inputs act as latched
/// parameters sampled at each activation; all-Value (or no) inputs make it
/// sync. Drained inputs (see [`PortDef::drained`]) complete before the
/// node's first activation and never color it async.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum ConsumptionMode {
    /// A total function over latched inputs: `Value`s and the completed
    /// totals of drained inputs.
    Sync,
    /// Driven by stream/event/future arrivals.
    Async,
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PortDef {
    /// The port's WIT kebab-case name.
    pub name: PortName,
    /// The port's delivery semantics.
    pub kind: PortKind,
    /// The payload type carried by the port.
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: Type,
    /// Input ports only: the port may be left unconnected. An unconnected
    /// optional Value reads as absent; an unconnected optional Event, Stream,
    /// or Future never delivers (and never colors an activation); an
    /// unconnected optional drained input latches an absent total without
    /// waiting.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub optional: bool,
    /// Input ports only: consumed to completion before the node's first
    /// activation, then latched as the completed total. Only Stream and
    /// Future inputs can be drained — Values and Events have no completion.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "core::ops::Not::not")
    )]
    pub drained: bool,
    /// Doc comment from the WIT field, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub docs: Option<String>,
}

impl PortDef {
    /// A required, undocumented port.
    pub fn new(name: impl Into<PortName>, kind: PortKind, ty: Type) -> Self {
        Self {
            name: name.into(),
            kind,
            ty,
            optional: false,
            drained: false,
            docs: None,
        }
    }

    /// Marks the port as safe to leave unconnected (inputs only).
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Marks the input as drained: consumed to completion before the node's
    /// first activation.
    pub fn drained(mut self) -> Self {
        self.drained = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_only_match_themselves() {
        let kinds = [
            PortKind::Value,
            PortKind::Event,
            PortKind::Stream,
            PortKind::Future,
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
        assert_eq!(PortKind::Event.to_string(), "event");
        assert_eq!(PortKind::Stream.to_string(), "stream");
        assert_eq!(PortKind::Future.to_string(), "future");
        assert_eq!(PortDirection::Input.to_string(), "input");
        assert_eq!(PortDirection::Output.to_string(), "output");
    }

    #[test]
    fn consumption_modes() {
        assert_eq!(PortKind::Value.consumption_mode(), ConsumptionMode::Sync);
        assert_eq!(PortKind::Event.consumption_mode(), ConsumptionMode::Async);
        assert_eq!(PortKind::Stream.consumption_mode(), ConsumptionMode::Async);
        assert_eq!(PortKind::Future.consumption_mode(), ConsumptionMode::Async);
    }
}
