//! Stable identifiers for nodes, ports, connections, and components, and
//! the references to WIT packages and worlds.
//!
//! IDs are human-assigned string slugs: diffable, deterministic across
//! editor sessions and serialization round-trips. Uniqueness within a graph
//! is a validation check, not a type invariant.

use core::fmt;
use core::str::FromStr;

// nutype can't feature-gate individual derives, so the whole attribute is
// duplicated under complementary cfg_attr conditions.
macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[cfg_attr(
            feature = "serde",
            nutype::nutype(derive(
                Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Display, From, Deref,
                Serialize, Deserialize
            ))
        )]
        #[cfg_attr(
            not(feature = "serde"),
            nutype::nutype(derive(
                Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Display, From, Deref
            ))
        )]
        pub struct $name(String);
    };
}

string_id! {
    /// Identifies a node within a graph.
    NodeId
}

string_id! {
    /// The name of a port on a component — kebab-case by WIT convention,
    /// though not validated here (ports lowered from WIT are already valid
    /// WIT names; hand-built contracts carry whatever they declare).
    PortName
}

string_id! {
    /// Identifies a connection within a graph.
    ConnectionId
}

string_id! {
    /// Identifies a link within a graph.
    LinkId
}

string_id! {
    /// Identifies a named resource pool for scheduling budgets.
    ResourceId
}

/// A reference to one port on one node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct PortRef {
    /// The node the port belongs to.
    pub node: NodeId,
    /// The port's name on that node's component.
    pub port: PortName,
}

impl PortRef {
    /// A reference to `port` on `node`.
    pub fn new(node: impl Into<NodeId>, port: impl Into<PortName>) -> Self {
        Self {
            node: node.into(),
            port: port.into(),
        }
    }
}

impl fmt::Display for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.node, self.port)
    }
}

/// A WIT package, as `namespace:name@version` (the version optional): what
/// a node manifest's `wit` names, and the package of every world it
/// provides.
///
/// Written and read as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct WitRef {
    /// The package namespace (`demo` in `demo:graph`).
    pub namespace: String,
    /// The package name (`graph` in `demo:graph`).
    pub name: String,
    /// The package version, when the WIT declares one.
    pub version: Option<semver::Version>,
}

impl fmt::Display for WitRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.name)?;
        if let Some(version) = &self.version {
            write!(f, "@{version}")?;
        }
        Ok(())
    }
}

impl FromStr for WitRef {
    type Err = ParseRefError;

    /// Parses `namespace:name[@version]`, namespace and name WIT
    /// identifiers.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseRefError::new("WIT package", s);
        let (s_ref, version) = split_version(s).ok_or_else(err)?;
        let (namespace, name) = s_ref.split_once(':').ok_or_else(err)?;
        if !is_kebab_ident(namespace) || !is_kebab_ident(name) {
            return Err(err());
        }
        Ok(Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version,
        })
    }
}

/// A world, as `namespace:name/world@version` (the version optional): the
/// contract a node fulfils, from the package `namespace:name@version`.
///
/// Written and read as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct WorldRef {
    /// The WIT package the world lives in.
    pub package: WitRef,
    /// The world's name within the package.
    pub world: String,
}

impl WorldRef {
    /// The world `world` of `package`.
    pub fn new(package: WitRef, world: impl Into<String>) -> Self {
        Self {
            package,
            world: world.into(),
        }
    }
}

impl fmt::Display for WorldRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}/{}",
            self.package.namespace, self.package.name, self.world
        )?;
        if let Some(version) = &self.package.version {
            write!(f, "@{version}")?;
        }
        Ok(())
    }
}

impl FromStr for WorldRef {
    type Err = ParseRefError;

    /// Parses `namespace:name/world[@version]`, namespace, name and world
    /// WIT identifiers.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseRefError::new("world", s);
        let (s_ref, version) = split_version(s).ok_or_else(err)?;
        let (namespace, rest) = s_ref.split_once(':').ok_or_else(err)?;
        let (name, world) = rest.split_once('/').ok_or_else(err)?;
        if !is_kebab_ident(namespace) || !is_kebab_ident(name) || !is_kebab_ident(world) {
            return Err(err());
        }
        Ok(Self {
            package: WitRef {
                namespace: namespace.to_string(),
                name: name.to_string(),
                version,
            },
            world: world.to_string(),
        })
    }
}

/// `s` split at its `@version`, the version parsed: `None` if it does not
/// parse.
fn split_version(s: &str) -> Option<(&str, Option<semver::Version>)> {
    match s.split_once('@') {
        Some((rest, version)) => Some((rest, Some(semver::Version::parse(version).ok()?))),
        None => Some((s, None)),
    }
}

/// A component's name in a graph's component table: a WIT identifier
/// (kebab-case), naming the node manifest it is described by.
///
/// Written and read as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct ComponentName(String);

impl ComponentName {
    /// The name, as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ComponentName {
    type Err = ParseRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if is_kebab_ident(s) {
            Ok(Self(s.to_string()))
        } else {
            Err(ParseRefError::new("component name", s))
        }
    }
}

impl core::borrow::Borrow<str> for ComponentName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

macro_rules! via_string {
    ($($name:ident),*) => {$(
        impl TryFrom<String> for $name {
            type Error = ParseRefError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
        impl TryFrom<&str> for $name {
            type Error = ParseRefError;
            fn try_from(s: &str) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.to_string()
            }
        }
    )*};
}
via_string!(WitRef, WorldRef, ComponentName);

/// What a contract is the contract of: one world, as one component
/// provides it. Shown as `component/world` (`adder/demo:math/adder@0.1.0`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ContractId {
    /// The component, by its name in the graph's table.
    pub component: ComponentName,
    /// The world it provides.
    pub world: WorldRef,
}

impl ContractId {
    /// `world`, as `component` provides it.
    pub fn new(component: ComponentName, world: WorldRef) -> Self {
        Self { component, world }
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.component, self.world)
    }
}

impl FromStr for ContractId {
    type Err = ParseRefError;

    /// Parses `component/namespace:name/world[@version]`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (component, world) = s
            .split_once('/')
            .ok_or_else(|| ParseRefError::new("contract id", s))?;
        Ok(Self {
            component: component.parse()?,
            world: world.parse()?,
        })
    }
}

/// A string that is not the reference it should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRefError {
    what: &'static str,
    input: String,
}

impl ParseRefError {
    fn new(what: &'static str, input: &str) -> Self {
        Self {
            what,
            input: input.to_string(),
        }
    }
}

impl fmt::Display for ParseRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {:?}", self.what, self.input)
    }
}

impl core::error::Error for ParseRefError {}

/// A WIT identifier, as wit-parser validates it: non-empty parts joined by
/// single dashes, each part all lowercase or all uppercase (digits allowed
/// in either). The first part starts with a letter; later parts may start
/// with a digit (`stage-2`, `HTTP-proxy`).
fn is_kebab_ident(s: &str) -> bool {
    s.split('-').enumerate().all(|(i, part)| {
        let starts_ok = match part.chars().next() {
            None => false,
            Some(first) => i > 0 || first.is_ascii_alphabetic(),
        };
        let lower = part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let upper = part
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        starts_ok && (lower || upper)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worlds_round_trip() {
        for s in ["demo:graph/sensor@0.1.0", "demo:graph/sensor"] {
            assert_eq!(s.parse::<WorldRef>().unwrap().to_string(), s);
        }
        let world: WorldRef = "demo:graph/sensor@0.1.0".parse().unwrap();
        assert_eq!(world.package.to_string(), "demo:graph@0.1.0");
        assert_eq!(world.world, "sensor");
    }

    #[test]
    fn packages_round_trip() {
        for s in ["wasi:cli@0.3.0", "demo:graph"] {
            assert_eq!(s.parse::<WitRef>().unwrap().to_string(), s);
        }
        assert!("wasi:cli/command@0.3.0".parse::<WitRef>().is_err());
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "demo",
            "demo:graph",
            "demo:graph/",
            ":graph/sensor",
            "demo:graph/sensor@nope",
            "Demo:graph/sensor",
            "demo:gr aph/sensor",
            "demo:graph/sen_sor",
            "demo:graph/-sensor",
            "demo:graph/sensor-",
            "demo:graph/se--nsor",
            "demo:graph/2sensor",
            "demo:graph/sen@sor@0.1.0",
            "demo:graph/Sensor",
            "demo:graph/sensor-Two",
            "demo:graph/sensor#ab12",
        ] {
            assert!(bad.parse::<WorldRef>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn parse_accepts_wit_idents() {
        for good in [
            "demo:graph/sensor-v2",
            "my-org:data-flow/edge-node@1.0.0",
            "demo:graph/stage-2",
            "demo:graph/HTTP-proxy",
            "demo:graph/sensor-4k",
        ] {
            assert!(good.parse::<WorldRef>().is_ok(), "rejected {good:?}");
        }
    }

    #[test]
    fn component_names_are_wit_identifiers() {
        assert!("adder".parse::<ComponentName>().is_ok());
        assert!("http-get-2".parse::<ComponentName>().is_ok());
        for bad in ["", "Adder2x", "a/b", "a:b", "a_b"] {
            assert!(bad.parse::<ComponentName>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn contract_ids_round_trip() {
        let id: ContractId = "adder/demo:math/adder@0.1.0".parse().unwrap();
        assert_eq!(id.component.as_str(), "adder");
        assert_eq!(id.world.to_string(), "demo:math/adder@0.1.0");
        assert_eq!(id.to_string(), "adder/demo:math/adder@0.1.0");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn references_are_strings() {
        let world: WorldRef = serde_json::from_str(r#""demo:graph/w@0.1.0""#).unwrap();
        assert_eq!(
            serde_json::to_string(&world).unwrap(),
            r#""demo:graph/w@0.1.0""#
        );
        assert!(serde_json::from_str::<WorldRef>(r#"{"world":"w"}"#).is_err());
        assert!(serde_json::from_str::<ComponentName>(r#""Not A Name""#).is_err());
    }

    #[test]
    fn port_ref_display() {
        assert_eq!(PortRef::new("n1", "out").to_string(), "n1.out");
    }
}
