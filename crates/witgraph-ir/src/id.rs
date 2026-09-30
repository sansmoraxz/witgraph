//! Stable identifiers for nodes, ports, connections, and components.
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
    /// Identifies a named resource pool for scheduling budgets.
    ResourceId
}

/// A reference to one port on one node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// A WIT package name, mirroring `wit_parser::PackageName`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PackageRef {
    /// The package namespace (`demo` in `demo:graph`).
    pub namespace: String,
    /// The package name (`graph` in `demo:graph`).
    pub name: String,
    /// The package version, when the WIT declares one.
    pub version: Option<semver::Version>,
}

/// Renders as `namespace:name@version` (version omitted when absent).
impl fmt::Display for PackageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.name)?;
        if let Some(version) = &self.version {
            write!(f, "@{version}")?;
        }
        Ok(())
    }
}

/// Component version identity: which WIT world, from which package version,
/// with an optional content hash of the lowered contract.
///
/// Version compatibility is exact-match only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComponentRef {
    /// The WIT package the world lives in.
    pub package: PackageRef,
    /// The world's name within the package.
    pub world: String,
    /// sha-256 hex of the lowered contract, when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub content_hash: Option<String>,
}

impl ComponentRef {
    /// Equality that ignores `content_hash` when either side lacks one.
    /// Derived `Eq` stays strict.
    ///
    /// The asymmetry with versions is deliberate: a version (`None` included)
    /// must match exactly, because it is always author-assigned; a hash is
    /// derived and may simply not have been computed yet, so its absence is
    /// treated as "unknown", not "different".
    pub fn matches(&self, other: &ComponentRef) -> bool {
        if self.package != other.package || self.world != other.world {
            return false;
        }
        match (&self.content_hash, &other.content_hash) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

/// Renders as `namespace:name/world@version` (hash excluded).
impl fmt::Display for ComponentRef {
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

/// The rejected input of a failed [`ComponentRef`] parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseComponentRefError(String);

impl fmt::Display for ParseComponentRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid component reference: {}", self.0)
    }
}

impl core::error::Error for ParseComponentRefError {}

/// A WIT kebab-case identifier: lowercase words of `[a-z][a-z0-9]*` joined
/// by single dashes.
fn is_kebab_ident(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|word| {
            word.starts_with(|c: char| c.is_ascii_lowercase())
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

impl FromStr for ComponentRef {
    type Err = ParseComponentRefError;

    /// Parses `namespace:name/world[@version]`, where namespace, name, and
    /// world must be WIT kebab-case identifiers.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseComponentRefError(s.to_string());
        let (namespace, rest) = s.split_once(':').ok_or_else(err)?;
        let (name, rest) = rest.split_once('/').ok_or_else(err)?;
        let (world, version) = match rest.split_once('@') {
            Some((world, ver)) => {
                let version = semver::Version::parse(ver).map_err(|_| err())?;
                (world, Some(version))
            }
            None => (rest, None),
        };
        if !is_kebab_ident(namespace) || !is_kebab_ident(name) || !is_kebab_ident(world) {
            return Err(err());
        }
        Ok(ComponentRef {
            package: PackageRef {
                namespace: namespace.to_string(),
                name: name.to_string(),
                version,
            },
            world: world.to_string(),
            content_hash: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_ref(version: Option<&str>, hash: Option<&str>) -> ComponentRef {
        ComponentRef {
            package: PackageRef {
                namespace: "demo".into(),
                name: "graph".into(),
                version: version.map(|v| semver::Version::parse(v).unwrap()),
            },
            world: "sensor".into(),
            content_hash: hash.map(String::from),
        }
    }

    #[test]
    fn display_and_parse_round_trip() {
        let with_version = component_ref(Some("0.1.0"), None);
        assert_eq!(with_version.to_string(), "demo:graph/sensor@0.1.0");
        assert_eq!(
            "demo:graph/sensor@0.1.0".parse::<ComponentRef>().unwrap(),
            with_version
        );

        let without_version = component_ref(None, None);
        assert_eq!(without_version.to_string(), "demo:graph/sensor");
        assert_eq!(
            "demo:graph/sensor".parse::<ComponentRef>().unwrap(),
            without_version
        );
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
        ] {
            assert!(bad.parse::<ComponentRef>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn parse_accepts_kebab_idents() {
        for good in ["demo:graph/sensor-v2", "my-org:data-flow/edge-node@1.0.0"] {
            assert!(good.parse::<ComponentRef>().is_ok(), "rejected {good:?}");
        }
    }

    #[test]
    fn package_ref_display() {
        let package = component_ref(Some("0.1.0"), None).package;
        assert_eq!(package.to_string(), "demo:graph@0.1.0");
        let versionless = component_ref(None, None).package;
        assert_eq!(versionless.to_string(), "demo:graph");
    }

    #[test]
    fn matches_ignores_missing_hash() {
        let unhashed = component_ref(Some("0.1.0"), None);
        let hashed = component_ref(Some("0.1.0"), Some("abc"));
        let other_hash = component_ref(Some("0.1.0"), Some("def"));

        assert!(unhashed.matches(&hashed));
        assert!(hashed.matches(&unhashed));
        assert!(hashed.matches(&hashed));
        assert!(!hashed.matches(&other_hash));
        assert_ne!(unhashed, hashed);

        let other_world = ComponentRef {
            world: "filter".into(),
            ..unhashed.clone()
        };
        assert!(!unhashed.matches(&other_world));
    }

    #[test]
    fn port_ref_display() {
        assert_eq!(PortRef::new("n1", "out").to_string(), "n1.out");
    }
}
