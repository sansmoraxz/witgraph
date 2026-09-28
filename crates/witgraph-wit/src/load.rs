//! Loading WIT sources into a resolved form.

use std::path::{Path, PathBuf};

use wit_parser::{PackageId, PackageName, Resolve, UnresolvedPackageGroup};

/// A resolved WIT source: the shared type arena plus the root packages that
/// were pushed (dependencies are reachable through `resolve`).
pub struct WitSource {
    /// The shared type arena for everything that was loaded.
    pub resolve: Resolve,
    /// The root packages that were pushed — the main package and any packages
    /// nested inside it — excluding dependencies.
    pub packages: Vec<PackageId>,
}

/// Failure while loading or resolving a WIT source.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// Reading the WIT file from disk failed.
    #[error("failed to read `{}`", path.display())]
    Read {
        /// The unreadable path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The WIT text failed to parse. The message is wit-parser's rendered
    /// diagnostic, source location included.
    #[error("{rendered}")]
    Parse {
        /// The rendered parse diagnostic.
        rendered: String,
    },
    /// The parsed packages failed to resolve (for example, a missing
    /// dependency or a cross-package inconsistency).
    #[error("failed to resolve WIT: {message}")]
    Resolve {
        /// wit-parser's flattened error chain.
        message: String,
    },
    /// A root package parsed from the source did not survive resolution.
    #[error("resolved WIT is missing root package `{name}`")]
    MissingRootPackage {
        /// The missing package.
        name: PackageName,
    },
}

fn new_resolve() -> Resolve {
    // all_features: don't silently filter @unstable-gated items (async
    // stream/future syntax may be feature-gated depending on wit-parser
    // version).
    Resolve {
        all_features: true,
        ..Resolve::default()
    }
}

fn parse_group(name: &str, wit: &str) -> Result<UnresolvedPackageGroup, LoadError> {
    UnresolvedPackageGroup::parse(name, wit).map_err(|(map, err)| LoadError::Parse {
        rendered: err.render(&map),
    })
}

/// The main package's name followed by the names of any nested packages.
fn root_names(group: &UnresolvedPackageGroup) -> Vec<PackageName> {
    std::iter::once(group.main.name.clone())
        .chain(group.nested.iter().map(|nested| nested.name.clone()))
        .collect()
}

/// Post-resolution ids for the given root package names.
fn root_ids(resolve: &Resolve, names: Vec<PackageName>) -> Result<Vec<PackageId>, LoadError> {
    names
        .into_iter()
        .map(|name| {
            resolve
                .package_names
                .get(&name)
                .copied()
                .ok_or_else(|| LoadError::MissingRootPackage { name })
        })
        .collect()
}

/// wit-parser reports resolution failures as an opaque error chain; flatten
/// it into the [`LoadError::Resolve`] message.
fn resolve_error(err: impl core::fmt::Display) -> LoadError {
    LoadError::Resolve {
        message: format!("{err:#}"),
    }
}

/// Load from a `.wit` file or a directory (with optional `deps/` folder).
/// Packages nested inside the source become roots alongside the main package.
pub fn load_path(path: impl AsRef<Path>) -> Result<WitSource, LoadError> {
    let path = path.as_ref();
    // Parse once up front to learn the root package names (main + nested);
    // `push_path` resolves nested packages but reports only the main one.
    let group = if path.is_dir() {
        UnresolvedPackageGroup::parse_dir(path).map_err(|err| LoadError::Parse {
            rendered: format!("{err:#}"),
        })?
    } else {
        let contents = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        parse_group(&path.display().to_string(), &contents)?
    };
    let names = root_names(&group);
    let mut resolve = new_resolve();
    resolve.push_path(path).map_err(resolve_error)?;
    let packages = root_ids(&resolve, names)?;
    Ok(WitSource { resolve, packages })
}

/// Load a single WIT document from a string; `name` is used in error
/// messages as the pseudo-path. Packages nested inside the document become
/// roots alongside the main package.
pub fn load_str(name: &str, wit: &str) -> Result<WitSource, LoadError> {
    let group = parse_group(name, wit)?;
    let names = root_names(&group);
    let mut resolve = new_resolve();
    resolve.push_group(group).map_err(resolve_error)?;
    let packages = root_ids(&resolve, names)?;
    Ok(WitSource { resolve, packages })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wit-parser must accept `stream<T>`/`future<T>` in record fields —
    /// the records-as-ports convention depends on it.
    #[test]
    fn stream_and_future_parse_in_record_fields() {
        let source = load_str(
            "spike.wit",
            r#"
            package demo:spike@0.1.0;

            interface node {
                record inputs {
                    samples: stream<f64>,
                    done: future<string>,
                    rate: option<u32>,
                    bare: future,
                }
            }

            world w {
                export node;
            }
            "#,
        )
        .expect("stream/future must parse inside record fields");
        assert_eq!(source.packages.len(), 1);
    }

    #[test]
    fn nested_packages_become_roots() {
        let source = load_str(
            "nested.wit",
            r#"
            package demo:outer@0.1.0;

            package demo:inner@0.1.0 {
                interface i { ping: func(); }
            }

            world w { import demo:inner/i@0.1.0; }
            "#,
        )
        .expect("nested packages must load");
        assert_eq!(source.packages.len(), 2);
        let names: Vec<String> = source
            .packages
            .iter()
            .map(|&id| source.resolve.packages[id].name.to_string())
            .collect();
        assert_eq!(names, ["demo:outer@0.1.0", "demo:inner@0.1.0"]);
    }

    #[test]
    fn load_path_accepts_a_single_file() {
        let source = load_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/demo/accumulator.wit"
        ))
        .expect("single-file WIT must load");
        assert_eq!(source.packages.len(), 1);
    }

    #[test]
    fn parse_errors_carry_the_rendered_location() {
        let Err(err) = load_str("broken.wit", "package demo:broken@0.1.0;\nnonsense") else {
            panic!("broken WIT must fail to parse");
        };
        let message = err.to_string();
        assert!(matches!(err, LoadError::Parse { .. }), "{message}");
        assert!(
            message.contains("broken.wit"),
            "rendered diagnostic must name the source: {message}"
        );
    }
}
