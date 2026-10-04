//! Loading WIT sources into a resolved form.

use std::path::{Path, PathBuf};

use wit_parser::{PackageId, PackageName, ParseError, Resolve, SourceMap, UnresolvedPackageGroup};

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
    /// WIT text failed to parse: the source's own, or a dependency's. The
    /// message is wit-parser's rendered diagnostic, source location
    /// included.
    #[error("{rendered}")]
    Parse {
        /// The rendered parse diagnostic.
        rendered: String,
    },
    /// The source parsed but failed to resolve (for example, a missing
    /// dependency or a cross-package inconsistency), or a wasm-encoded
    /// package failed to decode.
    #[error("failed to resolve WIT: {message}")]
    Resolve {
        /// wit-parser's error, rendered with its source location when it
        /// has one.
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

/// Whether `bytes` are a wasm-encoded WIT package (binary or text form)
/// rather than WIT text, as [`Resolve::push_path`] decides for a file.
fn is_wasm(path: &Path, bytes: &[u8]) -> bool {
    bytes.starts_with(b"\0asm")
        || path
            .extension()
            .is_some_and(|ext| ext == "wasm" || ext == "wat")
}

/// The names of the main package parsed from WIT text at `path` (a file or
/// a directory of `.wit` files) and of the packages nested in it; `None`
/// for a wasm-encoded package, which nests none (its dependencies come
/// with it, as dependencies).
fn main_group_names(path: &Path) -> Result<Option<Vec<PackageName>>, LoadError> {
    let mut map = SourceMap::default();
    let read = |message: String| LoadError::Resolve { message };
    if path.is_dir() {
        map.push_dir(path).map_err(|e| read(format!("{e:#}")))?;
    } else {
        let bytes = std::fs::read(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if is_wasm(path, &bytes) {
            return Ok(None);
        }
        map.push_file(path).map_err(|e| read(format!("{e:#}")))?;
    }
    match map.parse() {
        Ok(group) => Ok(Some(root_names(&group))),
        Err((map, err)) => Err(LoadError::Parse {
            rendered: err.render(&map),
        }),
    }
}

/// Load from a `.wit` file, a directory (with an optional `deps/` folder),
/// or a wasm-encoded WIT package: whatever [`Resolve::push_path`] accepts.
/// Packages nested inside the root source become roots alongside the main
/// package. Errors keep their source locations (`file:line:col`), in
/// dependencies too.
pub fn load_path(path: impl AsRef<Path>) -> Result<WitSource, LoadError> {
    let path = path.as_ref();
    // A missing or unreadable path is a read error, not a parse error.
    std::fs::metadata(path).map_err(|source| LoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut resolve = new_resolve();
    let main = match resolve.push_path(path) {
        Ok((main, _)) => main,
        Err(err) => {
            let rendered = resolve.render_error(&err);
            let syntax = err
                .chain()
                .any(|layer| layer.downcast_ref::<ParseError>().is_some());
            return Err(if syntax {
                LoadError::Parse { rendered }
            } else {
                LoadError::Resolve { message: rendered }
            });
        }
    };
    // The roots are the main package and the packages nested in its own
    // files, read off its parse (which just succeeded), as `load_str` does.
    // Dependencies, from `deps/` or wasm-encoded, are never roots.
    let packages = match main_group_names(path)? {
        Some(names) => root_ids(&resolve, names)?,
        None => vec![main],
    };
    Ok(WitSource { resolve, packages })
}

/// Load a single WIT document from a string; `name` is used in error
/// messages as the pseudo-path. Packages nested inside the document become
/// roots alongside the main package.
pub fn load_str(name: &str, wit: &str) -> Result<WitSource, LoadError> {
    let group = parse_group(name, wit)?;
    let names = root_names(&group);
    let mut resolve = new_resolve();
    if let Err(err) = resolve.push_group(group) {
        return Err(LoadError::Resolve {
            message: err.render(&resolve.source_map),
        });
    }
    let packages = root_ids(&resolve, names)?;
    Ok(WitSource { resolve, packages })
}

/// Load the WIT embedded in an encoded component: the world it was built
/// from, with the packages it depends on. The component's own package is
/// the root.
///
/// What it loads is the component's view of its world, which can be
/// narrower than the WIT source: a component imports only the functions
/// and resources its code uses.
///
/// The bytes are validated first ([`check_decodable`]): wit-parser's
/// decoder expects a valid component and panics on some bytes that are not
/// one. A panic the check does not foresee is still an error where panics
/// unwind; where they abort (on `wasm32`, say) only the check stands
/// between untrusted bytes and an abort.
///
/// [`check_decodable`]: crate::verify::check_decodable
pub fn load_component(bytes: &[u8]) -> Result<WitSource, LoadError> {
    let failed = |message: String| LoadError::Resolve { message };
    crate::verify::check_decodable(bytes).map_err(failed)?;
    let decoded = std::panic::catch_unwind(|| wit_parser::decoding::decode(bytes))
        .map_err(|_| failed("the component's WIT could not be decoded".into()))?
        .map_err(|e| failed(format!("{e:#}")))?;
    let package = decoded.package();
    let wit_parser::decoding::DecodedWasm::Component(resolve, _) = decoded else {
        return Err(failed(
            "the bytes are a WIT package, not a component".to_string(),
        ));
    };
    Ok(WitSource {
        resolve,
        packages: vec![package],
    })
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

    /// A fresh directory under the system temp dir, removed when dropped
    /// (a failing test included).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "witgraph-load-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    fn names(source: &WitSource) -> Vec<String> {
        let mut names: Vec<String> = source
            .packages
            .iter()
            .map(|&id| source.resolve.packages[id].name.to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn load_path_keeps_nested_packages_as_roots() {
        let dir = TempDir::new("nested");
        let file = dir.join("main.wit");
        std::fs::write(
            &file,
            r#"
            package demo:outer@0.1.0;
            package demo:inner@0.1.0 {
                interface i { ping: func(); }
            }
            world w { import demo:inner/i@0.1.0; }
            "#,
        )
        .unwrap();
        let source = load_path(&file).expect("loads");
        assert_eq!(names(&source), ["demo:inner@0.1.0", "demo:outer@0.1.0"]);
        let source = load_path(&*dir).expect("loads as a directory too");
        assert_eq!(names(&source), ["demo:inner@0.1.0", "demo:outer@0.1.0"]);
    }

    #[test]
    fn wasm_encoded_dependencies_are_not_roots() {
        // demo:lib, wasm-encoded, with a component world of its own.
        let mut lib = new_resolve();
        let lib_id = lib
            .push_str(
                "lib.wit",
                "package demo:lib@0.1.0;\n\
                 interface types { type sample = u32; }\n\
                 world libnode { export node: interface { record outputs { out: u32 } run: async func() -> outputs; } }\n",
            )
            .unwrap();
        let encoded = wit_component::encode(&lib, lib_id).unwrap();
        let dir = TempDir::new("wasm-dep");
        std::fs::create_dir_all(dir.join("deps")).unwrap();
        std::fs::write(dir.join("deps/lib.wasm"), encoded).unwrap();
        std::fs::write(
            dir.join("main.wit"),
            "package demo:app@0.1.0;\n\
             world app {\n\
               use demo:lib/types@0.1.0.{sample};\n\
               export node: interface { record outputs { out: u32 } run: async func() -> outputs; }\n\
             }\n",
        )
        .unwrap();
        let source = load_path(&*dir).expect("loads");
        assert_eq!(names(&source), ["demo:app@0.1.0"]);
        let ids: Vec<String> = crate::load_components(&*dir)
            .expect("lowers")
            .iter()
            .map(|c| c.id.to_string())
            .collect();
        assert_eq!(ids, ["demo:app/app@0.1.0"]);
    }

    #[test]
    fn syntax_errors_are_parse_errors_wherever_they_are() {
        let dir = TempDir::new("syntax");
        std::fs::write(
            dir.join("main.wit"),
            "package demo:app@0.1.0;\nworld w { nonsense }\n",
        )
        .unwrap();
        let Err(err) = load_path(&*dir) else {
            panic!("the main package has a syntax error");
        };
        assert!(matches!(err, LoadError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("main.wit:2"), "{err}");
    }

    #[test]
    fn load_str_resolution_errors_keep_their_location() {
        let Err(err) = load_str(
            "app.wit",
            "package demo:app@0.1.0;\n\nworld w { import demo:missing/x@0.1.0; }\n",
        ) else {
            panic!("the dependency is missing");
        };
        assert!(matches!(err, LoadError::Resolve { .. }), "{err}");
        assert!(err.to_string().contains("app.wit:3"), "{err}");
    }

    #[test]
    fn errors_in_dependencies_keep_their_location() {
        let dir = TempDir::new("deps");
        std::fs::create_dir_all(dir.join("deps")).unwrap();
        std::fs::write(
            dir.join("main.wit"),
            "package demo:app@0.1.0;\nworld w { import demo:caps/clock@0.1.0; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("deps/caps.wit"),
            "package demo:caps@0.1.0;\ninterface clock { now: func() -> u64 }\n",
        )
        .unwrap();
        let Err(err) = load_path(&*dir) else {
            panic!("the dependency has a syntax error");
        };
        let message = err.to_string();
        assert!(matches!(err, LoadError::Parse { .. }), "{message}");
        assert!(message.contains("caps.wit:2"), "{message}");
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
