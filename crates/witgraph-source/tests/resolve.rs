#![cfg(test)]
#![allow(missing_docs)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use test_components::ECHO;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use witgraph_ir::{ComponentRef, Graph};
use witgraph_runtime::{Perf, RuntimeConfig, RuntimeGraph, TickResult, Val};
use witgraph_source::{ComponentSource, Digest, Location, Resolver, SourceError};

/// A fresh directory under the test target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn echo() -> ComponentRef {
    "test:echo/echo@0.1.0".parse().unwrap()
}

/// A read-only OCI registry holding artifacts in memory, counting the
/// requests it serves.
#[derive(Default)]
struct Registry {
    /// Manifests by `repository:tag`.
    manifests: HashMap<String, String>,
    /// Blobs by digest (`sha256:…`).
    blobs: HashMap<String, Vec<u8>>,
    requests: AtomicUsize,
}

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

impl Registry {
    /// Adds an artifact holding `layers` (media type and bytes).
    fn artifact(&mut self, name: &str, layers: &[(&str, &[u8])]) {
        let mut blob = |bytes: &[u8]| {
            let digest = Digest::of(bytes).to_string();
            self.blobs.insert(digest.clone(), bytes.to_vec());
            digest
        };
        let config = blob(b"{}");
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|(media_type, bytes)| {
                serde_json::json!({
                    "mediaType": media_type,
                    "digest": blob(bytes),
                    "size": bytes.len(),
                })
            })
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_TYPE,
            "config": {
                "mediaType": "application/vnd.wasm.config.v0+json",
                "digest": config,
                "size": 2,
            },
            "layers": layers,
        });
        self.manifests
            .insert(name.to_string(), manifest.to_string());
    }

    /// The response to `GET path`: content type and body.
    fn get(&self, path: &str) -> Option<(&str, Vec<u8>)> {
        let rest = path.strip_prefix("/v2/")?;
        if rest.is_empty() {
            return Some(("application/json", b"{}".to_vec()));
        }
        if let Some((repository, reference)) = rest.split_once("/manifests/") {
            let manifest = self.manifests.get(&format!("{repository}:{reference}"))?;
            return Some((MANIFEST_TYPE, manifest.clone().into_bytes()));
        }
        let (_, digest) = rest.split_once("/blobs/")?;
        Some(("application/octet-stream", self.blobs.get(digest)?.clone()))
    }

    /// Serves the registry on a local port, returning `host:port`.
    async fn serve(self) -> (String, Arc<Registry>) {
        let registry = Arc::new(self);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let served = registry.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let registry = served.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    registry.requests.fetch_add(1, Ordering::SeqCst);
                    let line = String::from_utf8_lossy(&request);
                    let mut words = line.split_whitespace();
                    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                    let (status, content_type, body) = match registry.get(path) {
                        Some((content_type, body)) => ("200 OK", content_type, body),
                        None => ("404 Not Found", "application/json", b"{}".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    if method != "HEAD" {
                        let _ = socket.write_all(&body).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        (host, registry)
    }
}

/// A registry holding the `echo` guest as `test/echo:1.0`.
async fn echo_registry() -> (String, Arc<Registry>) {
    let mut registry = Registry::default();
    registry.artifact("test/echo:1.0", &[("application/wasm", ECHO.wasm)]);
    registry.serve().await
}

fn resolver(host: &str) -> Resolver {
    Resolver::new().insecure_registries(vec![host.to_string()])
}

#[tokio::test]
async fn a_file_source_resolves_relative_to_the_base() {
    let dir = scratch("file-source");
    std::fs::write(dir.join("echo.wasm"), ECHO.wasm).unwrap();
    let sources = [ComponentSource::file(echo(), "echo.wasm")];
    let components = Resolver::new().base(&dir).resolve(&sources).await.unwrap();
    let resolved = components.get(&echo()).unwrap();
    assert_eq!(*resolved.bytes, *ECHO.wasm);
    assert_eq!(resolved.digest, Digest::of(ECHO.wasm));
    assert_eq!(components.len(), 1);

    let missing = [ComponentSource::file(echo(), "absent.wasm")];
    let err = Resolver::new().base(&dir).resolve(&missing).await.err();
    assert!(matches!(err, Some(SourceError::Read { .. })), "{err:?}");
}

#[tokio::test]
async fn a_pinned_digest_is_checked() {
    let dir = scratch("pinned");
    std::fs::write(dir.join("echo.wasm"), ECHO.wasm).unwrap();
    let pinned = |digest: Digest| [ComponentSource::file(echo(), "echo.wasm").pinned(digest)];
    let resolver = Resolver::new().base(&dir);
    resolver
        .resolve(&pinned(Digest::of(ECHO.wasm)))
        .await
        .expect("the pinned bytes");
    let err = resolver
        .resolve(&pinned(Digest::of(b"other")))
        .await
        .expect_err("other bytes are pinned");
    match &err {
        SourceError::DigestMismatch {
            expected,
            found,
            location,
            ..
        } => {
            assert_eq!(*expected, Digest::of(b"other"));
            assert_eq!(*found, Digest::of(ECHO.wasm));
            assert_eq!(*location, Location::File("echo.wasm".into()));
        }
        other => panic!("expected a digest mismatch, got {other}"),
    }
}

#[tokio::test]
async fn a_component_has_one_source() {
    let sources = [
        ComponentSource::file(echo(), "a.wasm"),
        ComponentSource::file(echo(), "b.wasm"),
    ];
    let err = Resolver::new().resolve(&sources).await.err();
    assert!(
        matches!(err, Some(SourceError::Duplicate { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_oci_source_pulls_the_component_layer() {
    let (host, registry) = echo_registry().await;
    let sources = [ComponentSource::oci(
        echo(),
        format!("{host}/test/echo:1.0"),
    )];
    let components = resolver(&host).resolve(&sources).await.unwrap();
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
    assert!(registry.requests.load(Ordering::SeqCst) > 0);

    let wrong = [sources[0].clone().pinned(Digest::of(b"other"))];
    let err = resolver(&host).resolve(&wrong).await.err();
    assert!(
        matches!(err, Some(SourceError::DigestMismatch { .. })),
        "{err:?}"
    );

    let absent = [ComponentSource::oci(
        echo(),
        format!("{host}/test/absent:1.0"),
    )];
    let err = resolver(&host).resolve(&absent).await.err();
    assert!(matches!(err, Some(SourceError::Pull { .. })), "{err:?}");

    let malformed = [ComponentSource::oci(echo(), "Not A Reference")];
    let err = resolver(&host).resolve(&malformed).await.err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_pinned_artifact_in_the_cache_is_not_pulled_again() {
    let (host, registry) = echo_registry().await;
    let cache = scratch("oci-cache");
    let resolver = resolver(&host).cache(&cache);
    let unpinned = [ComponentSource::oci(
        echo(),
        format!("{host}/test/echo:1.0"),
    )];

    // The first pull fills the cache.
    let first = resolver.resolve(&unpinned).await.unwrap();
    let digest = first.get(&echo()).unwrap().digest.clone();
    let pulled = registry.requests.load(Ordering::SeqCst);
    assert!(pulled > 0);
    let file = cache.join("sha256").join(format!("{}.wasm", digest.hex()));
    assert_eq!(std::fs::read(&file).unwrap(), ECHO.wasm);

    // Pinned, it is read from the cache: the registry is not asked.
    let pinned = [unpinned[0].clone().pinned(digest)];
    let again = resolver.resolve(&pinned).await.unwrap();
    assert_eq!(*again.get(&echo()).unwrap().bytes, *ECHO.wasm);
    assert_eq!(registry.requests.load(Ordering::SeqCst), pulled);

    // Unpinned, the tag may have moved: it is pulled.
    resolver.resolve(&unpinned).await.unwrap();
    assert!(registry.requests.load(Ordering::SeqCst) > pulled);

    // A damaged cache file is not trusted.
    std::fs::write(&file, b"damaged").unwrap();
    let pulled = registry.requests.load(Ordering::SeqCst);
    let repaired = resolver.resolve(&pinned).await.unwrap();
    assert_eq!(*repaired.get(&echo()).unwrap().bytes, *ECHO.wasm);
    assert!(registry.requests.load(Ordering::SeqCst) > pulled);
    assert_eq!(std::fs::read(&file).unwrap(), ECHO.wasm);
}

#[tokio::test]
async fn an_artifact_holds_exactly_one_component_layer() {
    let mut registry = Registry::default();
    registry.artifact(
        "test/two:1.0",
        &[
            ("application/wasm", ECHO.wasm),
            ("application/wasm", b"another"),
        ],
    );
    registry.artifact("test/none:1.0", &[]);
    registry.artifact(
        "test/signed:1.0",
        &[
            ("application/vnd.dev.sigstore.bundle+json", b"{}"),
            ("application/wasm", ECHO.wasm),
        ],
    );
    let (host, _registry) = registry.serve().await;
    let resolve = async |name: &str| {
        let sources = [ComponentSource::oci(
            echo(),
            format!("{host}/test/{name}:1.0"),
        )];
        resolver(&host).resolve(&sources).await.err()
    };
    let two = resolve("two").await;
    assert!(
        matches!(&two, Some(SourceError::Layers { found: 2, .. })),
        "{two:?}"
    );
    let none = resolve("none").await;
    assert!(
        matches!(&none, Some(SourceError::Layers { found: 0, .. })),
        "{none:?}"
    );
    // A layer of another media type is not the component's.
    let signed = [ComponentSource::oci(
        echo(),
        format!("{host}/test/signed:1.0"),
    )];
    let components = resolver(&host).resolve(&signed).await.unwrap();
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
}

#[tokio::test]
async fn a_graph_loads_from_resolved_artifacts_alone() {
    let (host, _registry) = echo_registry().await;
    let sources = [ComponentSource::oci(
        echo(),
        format!("{host}/test/echo:1.0"),
    )];
    let components = resolver(&host).resolve(&sources).await.unwrap();

    // The contract comes from the bytes: no WIT source is at hand.
    let bytes = &components.get(&echo()).unwrap().bytes;
    let contract = witgraph_wit::lower_component(bytes, &echo())
        .unwrap()
        .contract;
    assert!(echo().matches(&contract.id));
    assert!(contract.id.content_hash.is_some());
    let graph = Graph::builder("from-artifacts")
        .add_component(&contract)
        .add_node("n", contract.id.clone())
        .build();
    let compiled = graph
        .compile(std::slice::from_ref(&contract))
        .expect("compiles");

    // The bytes are keyed by the unhashed ref the source named.
    let mut rt = RuntimeGraph::load(compiled, &components.wasm(), RuntimeConfig::default(), Perf)
        .await
        .expect("loads");
    rt.inject(&"n".into(), &"in".into(), Val::Float64(2.5))
        .unwrap();
    for _ in 0..4 {
        if matches!(rt.tick().await, TickResult::Idle) {
            break;
        }
    }
    assert_eq!(
        rt.read_output(&"n".into(), &"out".into()).unwrap(),
        Some(Val::Float64(2.5))
    );
}

#[tokio::test]
async fn sources_sharing_an_artifact_pull_it_once() {
    let (host, registry) = echo_registry().await;
    let cache = scratch("shared-artifact");
    let reference = format!("{host}/test/echo:1.0");
    let hashed: ComponentRef = format!("{:#}#{}", echo(), "ab".repeat(32)).parse().unwrap();
    let sources = [
        ComponentSource::oci(echo(), reference.clone()),
        ComponentSource::oci(hashed.clone(), reference),
    ];
    // What one pull costs, on its own.
    resolver(&host).resolve(&sources[..1]).await.unwrap();
    let lone = registry.requests.load(Ordering::SeqCst);
    let components = resolver(&host)
        .cache(&cache)
        .resolve(&sources)
        .await
        .unwrap();
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
    assert_eq!(*components.get(&hashed).unwrap().bytes, *ECHO.wasm);
    assert_eq!(
        registry.requests.load(Ordering::SeqCst),
        2 * lone,
        "one pull serves both"
    );
    let cached: Vec<_> = std::fs::read_dir(cache.join("sha256"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(cached.len(), 1, "no partial file is left: {cached:?}");
}

#[tokio::test]
async fn a_component_over_the_size_limit_is_refused() {
    let dir = scratch("too-large");
    std::fs::write(dir.join("echo.wasm"), ECHO.wasm).unwrap();
    let file = [ComponentSource::file(echo(), "echo.wasm")];
    let small = Resolver::new().base(&dir).max_component_bytes(16);
    let err = small.resolve(&file).await.err();
    assert!(
        matches!(err, Some(SourceError::TooLarge { limit: 16, .. })),
        "{err:?}"
    );

    let (host, _registry) = echo_registry().await;
    let oci = [ComponentSource::oci(
        echo(),
        format!("{host}/test/echo:1.0"),
    )];
    let err = resolver(&host)
        .max_component_bytes(16)
        .resolve(&oci)
        .await
        .err();
    assert!(matches!(err, Some(SourceError::TooLarge { .. })), "{err:?}");
    let exact = ECHO.wasm.len() as u64;
    resolver(&host)
        .max_component_bytes(exact)
        .resolve(&oci)
        .await
        .expect("a component of exactly the limit is fine");
}

#[tokio::test]
async fn a_cache_that_cannot_be_written_does_not_fail_the_resolve() {
    let (host, _registry) = echo_registry().await;
    // A file where the cache directory should be: nothing can be kept.
    let blocked = scratch("blocked-cache").join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let sources = [ComponentSource::oci(
        echo(),
        format!("{host}/test/echo:1.0"),
    )];
    let components = resolver(&host)
        .cache(&blocked)
        .resolve(&sources)
        .await
        .expect("the bytes were pulled and checked");
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
}

#[tokio::test]
async fn a_file_source_stays_under_the_base() {
    let root = scratch("outside-base");
    let base = root.join("project");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(root.join("secret.wasm"), ECHO.wasm).unwrap();
    for path in ["../secret.wasm", root.join("secret.wasm").to_str().unwrap()] {
        let sources = [ComponentSource::file(echo(), path)];
        let err = Resolver::new().base(&base).resolve(&sources).await.err();
        assert!(
            matches!(err, Some(SourceError::OutsideBase { .. })),
            "{path}: {err:?}"
        );
    }
    let sources = [ComponentSource::file(echo(), "../secret.wasm")];
    let trusted = Resolver::new().base(&base).allow_paths_outside_base();
    let components = trusted.resolve(&sources).await.expect("a trusted list may");
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symbolic_link_out_of_the_base_is_refused() {
    let root = scratch("symlink-out");
    let base = root.join("project");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(root.join("secret.wasm"), ECHO.wasm).unwrap();
    std::os::unix::fs::symlink(root.join("secret.wasm"), base.join("evil.wasm")).unwrap();
    let sources = [ComponentSource::file(echo(), "evil.wasm")];
    let err = Resolver::new().base(&base).resolve(&sources).await.err();
    assert!(
        matches!(err, Some(SourceError::OutsideBase { .. })),
        "{err:?}"
    );
    // A link leading nowhere is not followed later, when something may
    // have appeared at its target: it fails now.
    std::os::unix::fs::symlink(root.join("later.wasm"), base.join("dangling.wasm")).unwrap();
    let sources = [ComponentSource::file(echo(), "dangling.wasm")];
    let err = Resolver::new().base(&base).resolve(&sources).await.err();
    assert!(matches!(err, Some(SourceError::Read { .. })), "{err:?}");
}

#[tokio::test]
async fn a_malformed_reference_fails_even_when_its_digest_is_cached() {
    let (host, _registry) = echo_registry().await;
    let cache = scratch("cached-bad-reference");
    let resolver = resolver(&host).cache(&cache);
    let good = ComponentSource::oci(echo(), format!("{host}/test/echo:1.0"));
    let digest = resolver.fetch(&good).await.unwrap().digest;
    let bad = ComponentSource::oci(echo(), "Not A Reference").pinned(digest);
    let err = resolver.fetch(&bad).await.err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}
