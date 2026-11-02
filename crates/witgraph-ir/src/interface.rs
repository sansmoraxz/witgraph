//! Interface ids (`namespace:name/interface@version`) and how their
//! versions match.

/// An interface id split into the id without its version, and the
/// version, if it has one.
fn split_version(id: &str) -> Option<(&str, semver::Version)> {
    let (base, version) = id.rsplit_once('@')?;
    Some((base, semver::Version::parse(version).ok()?))
}

/// The version of an interface id, if it has one.
pub fn interface_version(id: &str) -> Option<semver::Version> {
    split_version(id).map(|(_, version)| version)
}

/// Whether two interface ids name the same interface at semver-compatible
/// versions, the way wasmtime's linker matches imports: on the same
/// compatibility track (the same major from 1.0, the same minor for 0.x).
/// A 0.0.x or pre-release version only matches itself, exactly (as
/// wasmtime's linker does), and so does an id without a version.
pub fn semver_compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Some((base_a, va)), Some((base_b, vb))) = (split_version(a), split_version(b)) else {
        return false;
    };
    let exact_only = |v: &semver::Version| (v.major == 0 && v.minor == 0) || !v.pre.is_empty();
    if base_a != base_b || exact_only(&va) || exact_only(&vb) {
        return false;
    }
    track(&va) == track(&vb)
}

/// An interface id with its version cut to its compatibility track
/// (`@1` for 1.x, `@0.2` for 0.2.x), as [`semver_compatible`] matches
/// them: two ids are compatible iff their tracks are equal. An id without
/// a version, or at a 0.0.x or pre-release version, is its own track.
pub fn compat_track(id: &str) -> String {
    let Some((base, version)) = split_version(id) else {
        return id.to_string();
    };
    if (version.major == 0 && version.minor == 0) || !version.pre.is_empty() {
        return id.to_string();
    }
    match track(&version) {
        (0, minor) => format!("{base}@0.{minor}"),
        (major, _) => format!("{base}@{major}"),
    }
}

/// Whether an implementation of interface `provided` serves an import of
/// `wanted`: the same id, or the same interface at a semver-compatible
/// version no older than it.
pub fn semver_serves(provided: &str, wanted: &str) -> bool {
    semver_compatible(provided, wanted) && interface_version(provided) >= interface_version(wanted)
}

/// The compatibility track of a version, as WIT tooling has it: its major
/// from 1.0, its minor for 0.x. (A 0.0.x version matches only itself, and
/// never gets here.)
fn track(version: &semver::Version) -> (u64, u64) {
    match version.major {
        0 => (0, version.minor),
        major => (major, 0),
    }
}

/// The capability an import of `import` resolves to among `capabilities`:
/// the one of that very name, else the newest semver-compatible interface,
/// as wit-component merges such imports into one (and wasmtime's linker
/// resolves them). Compilation, verification and loading all decide with
/// this which import a link or an import is.
pub fn resolve_import<'c>(
    capabilities: &'c [crate::Capability],
    import: &str,
) -> Option<&'c crate::Capability> {
    capabilities
        .iter()
        .find(|c| c.interface == import)
        .or_else(|| {
            capabilities
                .iter()
                .filter(|c| c.is_interface() && semver_compatible(&c.interface, import))
                .max_by_key(|c| interface_version(&c.interface))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compatibility_follows_wasmtime() {
        assert!(semver_compatible("a:b/c@0.1.0", "a:b/c@0.1.3"));
        assert!(semver_compatible("a:b/c@1.2.0", "a:b/c@1.9.1"));
        assert!(!semver_compatible("a:b/c@0.1.0", "a:b/c@0.2.0"));
        assert!(!semver_compatible("a:b/c@1.0.0", "a:b/c@2.0.0"));
        assert!(!semver_compatible("a:b/c@0.0.1", "a:b/c@0.0.2"));
        assert!(!semver_compatible("a:b/c@0.1.0", "a:b/d@0.1.0"));
        assert!(
            semver_compatible("config", "config"),
            "without a version, an id matches only itself"
        );
        assert!(!semver_compatible("config", "other"));
    }

    #[test]
    fn compatible_ids_share_a_track() {
        assert_eq!(compat_track("a:b/c@0.1.3"), "a:b/c@0.1");
        assert_eq!(compat_track("a:b/c@1.9.1"), "a:b/c@1");
        assert_eq!(compat_track("a:b/c@0.0.2"), "a:b/c@0.0.2");
        assert_eq!(compat_track("a:b/c@1.0.0-rc.1"), "a:b/c@1.0.0-rc.1");
        assert_eq!(compat_track("config"), "config");
        let ids = [
            "a:b/c@0.1.0",
            "a:b/c@0.1.3",
            "a:b/c@0.2.0",
            "a:b/c@0.0.1",
            "a:b/c@1.2.0",
            "a:b/c@1.9.1",
        ];
        for a in ids {
            for b in ids {
                assert_eq!(
                    compat_track(a) == compat_track(b),
                    semver_compatible(a, b),
                    "{a} {b}"
                );
            }
        }
    }

    #[test]
    fn an_id_is_compatible_with_and_serves_itself() {
        for id in ["a:b/c@0.0.1", "a:b/c@1.0.0-rc.1", "config", "a:b/c@0.2.0"] {
            assert!(semver_compatible(id, id), "{id}");
            assert!(semver_serves(id, id), "{id}");
        }
        assert!(!semver_compatible("a:b/c@0.0.1", "a:b/c@0.0.2"));
        assert!(semver_serves("a:b/c@0.2.3", "a:b/c@0.2.0"));
        assert!(!semver_serves("a:b/c@0.2.0", "a:b/c@0.2.3"));
    }

    /// The rule is wac's (which composition uses) and wasmtime's: kept in
    /// step with wac's own copy here, without depending on it outside
    /// tests.
    #[test]
    fn semver_compatibility_is_wacs() {
        let versions = [
            "0.0.1",
            "0.0.2",
            "0.0.1+a",
            "0.1.0",
            "0.1.3",
            "0.1.3+build",
            "0.2.0",
            "1.0.0",
            "1.2.0",
            "1.9.1",
            "2.0.0",
            "1.0.0-rc.1",
            "1.0.0-rc.2",
            "0.1.0-alpha",
            "x",
        ];
        let mut ids: Vec<String> = ["a:b/c", "a:b/d"]
            .iter()
            .flat_map(|base| versions.iter().map(move |v| format!("{base}@{v}")))
            .collect();
        ids.extend(["config".into(), "other".into(), "a:b/c".into()]);
        for a in &ids {
            for b in &ids {
                assert_eq!(
                    semver_compatible(a, b),
                    wac_types::are_semver_compatible(a, b),
                    "{a} vs {b}"
                );
            }
        }
    }
}
