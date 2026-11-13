#![cfg(test)]
#![allow(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use test_components::{ECHO, RELAY};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use witgraph_ir::{ComponentName, ContractId, ContractSource, Graph};
use witgraph_runtime::{Perf, RunningGraph, RuntimeConfig, RuntimeGraph, TickResult, Val};
use witgraph_source::{
    Digest, Location, MANIFEST_MEDIA_TYPE, Resolver, SourceError, WASM_LAYER_MEDIA_TYPES,
};

/// A fresh directory under the test target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn echo() -> ComponentName {
    "echo".parse().unwrap()
}

const ECHO_WORLD: &str = "test:echo/echo@0.1.0";

/// A node manifest for `echo`, its bytes and WIT at the locations given.
fn manifest(component: &str, wit: &str, worlds: &[&str]) -> String {
    serde_json::json!({
        "wit": "test:echo@0.1.0",
        "wit_source": wit,
        "worlds": worlds,
        "component": component,
        "viewers": { ECHO_WORLD: { "out": "series" } },
    })
    .to_string()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A project holding `echo`: its bytes (`bytes`), WIT and node manifest
/// (`echo.node.json`).
fn project(name: &str, bytes: &[u8], worlds: &[&str]) -> PathBuf {
    let dir = scratch(name);
    std::fs::write(dir.join("echo.wasm"), bytes).unwrap();
    copy_dir(Path::new(ECHO.wit), &dir.join("wit"));
    std::fs::write(
        dir.join("echo.node.json"),
        manifest("echo.wasm", "wit", worlds),
    )
    .unwrap();
    dir
}

fn table(location: &str) -> BTreeMap<ComponentName, String> {
    BTreeMap::from([(echo(), location.to_string())])
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
    /// Stops it listening: a registry that cannot be reached.
    stop: tokio::sync::Notify,
}

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

impl Registry {
    /// Adds an artifact holding `layers` (media type and bytes), under the
    /// tag `name` (`repository:tag`) and under its digest; returns the
    /// digest.
    fn artifact(&mut self, name: &str, layers: &[(&str, &[u8])]) -> String {
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
        let manifest = manifest.to_string();
        let digest = Digest::of(manifest.as_bytes()).to_string();
        let (repository, _) = name.split_once(':').unwrap();
        self.manifests
            .insert(format!("{repository}:{digest}"), manifest.clone());
        self.manifests.insert(name.to_string(), manifest);
        digest
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
        self.serve_at("127.0.0.1:0").await
    }

    /// Serves the registry at `address`, returning `host:port`.
    async fn serve_at(self, address: &str) -> (String, Arc<Registry>) {
        let registry = Arc::new(self);
        let listener = TcpListener::bind(address).await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let served = registry.clone();
        tokio::spawn(async move {
            loop {
                let mut socket = tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((socket, _)) => socket,
                        Err(_) => return,
                    },
                    () = served.stop.notified() => return,
                };
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

/// A registry holding `echo`: its bytes as `test/echo:1.0`, its WIT as
/// `test/echo-wit:1.0`, and its node manifest, naming those, as
/// `test/echo-node:1.0`. Returns the digests of the three artifacts too.
async fn echo_registry() -> (String, Arc<Registry>, [String; 3]) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    drop(listener);
    let mut registry = Registry::default();
    let wit = witgraph_wit::load::encode(&ECHO.wit_source()).unwrap();
    let bytes = registry.artifact("test/echo:1.0", &[("application/wasm", ECHO.wasm)]);
    let wit = registry.artifact("test/echo-wit:1.0", &[("application/wasm", &wit)]);
    let pinned =
        |repository: &str, digest: &str| format!("oci://{host}/test/{repository}@{digest}");
    let node = manifest(
        &pinned("echo", &bytes),
        &pinned("echo-wit", &wit),
        &[ECHO_WORLD],
    );
    let node = registry.artifact(
        "test/echo-node:1.0",
        &[(MANIFEST_MEDIA_TYPE, node.as_bytes())],
    );
    let (served, registry) = registry.serve_at(&host).await;
    assert_eq!(served, host);
    (host, registry, [bytes, wit, node])
}

fn resolver(host: &str) -> Resolver {
    Resolver::new().insecure_registries(vec![host.to_string()])
}

#[tokio::test]
async fn a_component_resolves_from_its_node_manifest() {
    let dir = project("file-manifest", ECHO.wasm, &[ECHO_WORLD]);
    let components = Resolver::new()
        .base(&dir)
        .resolve(&table("echo.node.json"))
        .await
        .unwrap();
    let echo_component = components.get(&echo()).unwrap();
    assert_eq!(*echo_component.bytes, *ECHO.wasm);
    assert_eq!(echo_component.digest, Digest::of(ECHO.wasm));
    let id: ContractId = format!("echo/{ECHO_WORLD}").parse().unwrap();
    let contract = components.contract(&id).expect("its node world's contract");
    assert_eq!(contract.outputs[0].viewer.as_deref(), Some("series"));
    assert!(contract.docs.is_some(), "docs come from the WIT");
    assert_eq!(components.len(), 1);

    let err = Resolver::new()
        .base(&dir)
        .resolve(&table("absent.node.json"))
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::In { error, .. }) if matches!(**error, SourceError::Read { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_manifest_says_what_its_bytes_are_and_is_held_to_it() {
    let resolve = async |name: &str, bytes: &[u8], worlds: &[&str]| {
        let dir = project(name, bytes, worlds);
        Resolver::new()
            .base(&dir)
            .component(&echo(), "echo.node.json")
            .await
            .err()
    };
    // A world of another package.
    let err = resolve("other-package", ECHO.wasm, &["test:relay/relay@0.1.0"]).await;
    assert!(matches!(err, Some(SourceError::World { .. })), "{err:?}");
    // A world its WIT does not have.
    let err = resolve("no-such-world", ECHO.wasm, &["test:echo/nope@0.1.0"]).await;
    assert!(matches!(err, Some(SourceError::World { .. })), "{err:?}");
    // Bytes that do not target the world.
    let err = resolve("other-bytes", RELAY.wasm, &[ECHO_WORLD]).await;
    assert!(matches!(err, Some(SourceError::Verify(_))), "{err:?}");
    // A manifest naming another package than its WIT is.
    let dir = project("other-wit", ECHO.wasm, &[ECHO_WORLD]);
    let other = manifest("echo.wasm", "wit", &[ECHO_WORLD]).replace(
        "\"wit\":\"test:echo@0.1.0\"",
        "\"wit\":\"test:other@0.1.0\"",
    );
    std::fs::write(dir.join("other.node.json"), other).unwrap();
    let err = Resolver::new()
        .base(&dir)
        .component(&echo(), "other.node.json")
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Wit { .. })), "{err:?}");
    // Not a manifest.
    std::fs::write(dir.join("bad.node.json"), b"{\"wit\": 1}").unwrap();
    let err = Resolver::new()
        .base(&dir)
        .component(&echo(), "bad.node.json")
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Manifest { .. })), "{err:?}");
}

#[tokio::test]
async fn an_oci_component_pulls_its_manifest_wit_and_bytes() {
    let (host, registry, _) = echo_registry().await;
    let components = resolver(&host)
        .resolve(&table(&format!("oci://{host}/test/echo-node:1.0")))
        .await
        .unwrap();
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
    assert!(registry.requests.load(Ordering::SeqCst) > 0);

    let err = resolver(&host)
        .component(&echo(), &format!("oci://{host}/test/absent:1.0"))
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Pull { .. })), "{err:?}");
    let err = resolver(&host)
        .component(&echo(), "oci://Not A Reference")
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_manifest_pulled_over_oci_names_only_artifacts() {
    let mut registry = Registry::default();
    let node = manifest("echo.wasm", "wit", &[ECHO_WORLD]);
    registry.artifact("test/loose:1.0", &[(MANIFEST_MEDIA_TYPE, node.as_bytes())]);
    let (host, _registry) = registry.serve().await;
    let err = resolver(&host)
        .component(&echo(), &format!("oci://{host}/test/loose:1.0"))
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::FileFromOci { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_pinned_reference_in_the_cache_is_not_pulled_again() {
    let (host, registry, [bytes, _, node]) = echo_registry().await;
    let cache = scratch("oci-cache");
    let resolver = resolver(&host).cache(&cache);
    let pinned = format!("oci://{host}/test/echo-node@{node}");

    // The first resolve fills the cache.
    let first = resolver.component(&echo(), &pinned).await.unwrap();
    let pulled = registry.requests.load(Ordering::SeqCst);
    assert!(pulled > 0);
    let file = cache
        .join("sha256")
        .join(format!("{}.wasm", first.digest.hex()));
    assert_eq!(std::fs::read(&file).unwrap(), ECHO.wasm);

    // Pinned, everything is read from the cache: the registry is not asked.
    let again = resolver.component(&echo(), &pinned).await.unwrap();
    assert_eq!(*again.bytes, *ECHO.wasm);
    assert_eq!(registry.requests.load(Ordering::SeqCst), pulled);

    // A tag may have moved: it is pulled.
    let tagged: Location = format!("oci://{host}/test/echo:1.0").parse().unwrap();
    assert!(!resolver.fetch(&tagged).await.unwrap().offline);
    assert!(registry.requests.load(Ordering::SeqCst) > pulled);

    // A damaged cache file is not trusted.
    std::fs::write(&file, b"damaged").unwrap();
    let pulled = registry.requests.load(Ordering::SeqCst);
    let bytes_pinned: Location = format!("oci://{host}/test/echo@{bytes}").parse().unwrap();
    let repaired = resolver.fetch(&bytes_pinned).await.unwrap();
    assert_eq!(*repaired.bytes, *ECHO.wasm);
    assert!(registry.requests.load(Ordering::SeqCst) > pulled);
    assert_eq!(std::fs::read(&file).unwrap(), ECHO.wasm);
}

#[tokio::test]
async fn a_tag_whose_registry_is_gone_is_what_it_held_last_marked_offline() {
    let (host, registry, _) = echo_registry().await;
    let cache = scratch("offline-tag");
    let tagged = table(&format!("oci://{host}/test/echo-node:1.0"));
    let online = resolver(&host)
        .cache(&cache)
        .resolve(&tagged)
        .await
        .unwrap();
    assert!(!online.get(&echo()).unwrap().offline);

    registry.stop.notify_one();
    let offline = resolver(&host)
        .cache(&cache)
        .resolve(&tagged)
        .await
        .unwrap();
    let offline = offline.get(&echo()).unwrap();
    assert!(offline.offline);
    assert_eq!(*offline.bytes, *ECHO.wasm);

    // With nothing cached, there is nothing to fall back on.
    let err = resolver(&host)
        .cache(scratch("offline-tag-empty"))
        .resolve(&tagged)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("failed to reach"), "{err}");
}

#[tokio::test]
async fn an_artifact_holds_exactly_one_layer_of_its_kind() {
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
            (WASM_LAYER_MEDIA_TYPES[1], ECHO.wasm),
        ],
    );
    let (host, _registry) = registry.serve().await;
    let fetch = async |name: &str| {
        let location: Location = format!("oci://{host}/test/{name}:1.0").parse().unwrap();
        resolver(&host).fetch(&location).await
    };
    let two = fetch("two").await.err();
    assert!(
        matches!(&two, Some(SourceError::Layers { found: 2, .. })),
        "{two:?}"
    );
    let none = fetch("none").await.err();
    assert!(
        matches!(&none, Some(SourceError::Layers { found: 0, .. })),
        "{none:?}"
    );
    // A layer of another media type is not the component's.
    assert_eq!(*fetch("signed").await.unwrap().bytes, *ECHO.wasm);
}

#[tokio::test]
async fn a_graph_loads_from_resolved_components() {
    let (host, _registry, _) = echo_registry().await;
    let table = table(&format!("oci://{host}/test/echo-node:1.0"));
    let components = resolver(&host).resolve(&table).await.unwrap();
    let mut graph = Graph::builder("from-artifacts")
        .add_node("n", format!("echo/{ECHO_WORLD}").parse().unwrap())
        .build();
    graph.components = table;
    let compiled = graph.compile(&components).expect("compiles");
    let mut rt = RuntimeGraph::load(
        compiled,
        &components.artifacts(),
        RuntimeConfig::default(),
        Perf,
    )
    .await
    .expect("loads");
    rt.inject(&"n".into(), &"in".into(), Val::Float64(2.5))
        .await
        .unwrap();
    for _ in 0..4 {
        if matches!(rt.tick().await, TickResult::Idle) {
            break;
        }
    }
    assert_eq!(
        rt.read_output(&"n".into(), &"out".into()).await.unwrap(),
        Some(Val::Float64(2.5))
    );
}

#[tokio::test]
async fn components_sharing_artifacts_resolve_alike() {
    let (host, _registry, _) = echo_registry().await;
    let location = format!("oci://{host}/test/echo-node:1.0");
    let table = BTreeMap::from([
        (echo(), location.clone()),
        ("other".parse().unwrap(), location),
    ]);
    let components = resolver(&host).resolve(&table).await.unwrap();
    assert_eq!(components.len(), 2);
    let other: ContractId = format!("other/{ECHO_WORLD}").parse().unwrap();
    assert!(components.contract(&other).is_some(), "named by the table");
}

#[tokio::test]
async fn a_component_over_the_size_limit_is_refused() {
    let dir = project("too-large", ECHO.wasm, &[ECHO_WORLD]);
    let file = Location::File("echo.wasm".into());
    let small = Resolver::new().base(&dir).max_component_bytes(16);
    let err = small.fetch(&file).await.err();
    assert!(
        matches!(err, Some(SourceError::TooLarge { limit: 16, .. })),
        "{err:?}"
    );

    let (host, _registry, _) = echo_registry().await;
    let oci: Location = format!("oci://{host}/test/echo:1.0").parse().unwrap();
    let err = resolver(&host)
        .max_component_bytes(16)
        .fetch(&oci)
        .await
        .err();
    assert!(matches!(err, Some(SourceError::TooLarge { .. })), "{err:?}");
    let exact = ECHO.wasm.len() as u64;
    resolver(&host)
        .max_component_bytes(exact)
        .fetch(&oci)
        .await
        .expect("a component of exactly the limit is fine");
}

#[tokio::test]
async fn a_cache_that_cannot_be_written_does_not_fail_the_resolve() {
    let (host, _registry, _) = echo_registry().await;
    // A file where the cache directory should be: nothing can be kept.
    let blocked = scratch("blocked-cache").join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let components = resolver(&host)
        .cache(&blocked)
        .resolve(&table(&format!("oci://{host}/test/echo-node:1.0")))
        .await
        .expect("the bytes were pulled and checked");
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
}

#[tokio::test]
async fn a_file_stays_under_the_base() {
    let root = scratch("outside-base");
    let base = root.join("project");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(root.join("secret.wasm"), ECHO.wasm).unwrap();
    for path in ["../secret.wasm", root.join("secret.wasm").to_str().unwrap()] {
        let err = Resolver::new()
            .base(&base)
            .fetch(&Location::File(path.into()))
            .await
            .err();
        assert!(
            matches!(err, Some(SourceError::OutsideBase { .. })),
            "{path}: {err:?}"
        );
    }
    let trusted = Resolver::new().base(&base).allow_paths_outside_base();
    let fetched = trusted
        .fetch(&Location::File("../secret.wasm".into()))
        .await
        .expect("a trusted table may");
    assert_eq!(*fetched.bytes, *ECHO.wasm);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symbolic_link_out_of_the_base_is_refused() {
    let root = scratch("symlink-out");
    let base = root.join("project");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(root.join("secret.wasm"), ECHO.wasm).unwrap();
    std::os::unix::fs::symlink(root.join("secret.wasm"), base.join("evil.wasm")).unwrap();
    let err = Resolver::new()
        .base(&base)
        .fetch(&Location::File("evil.wasm".into()))
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::OutsideBase { .. })),
        "{err:?}"
    );
    // A link leading nowhere is not followed later, when something may
    // have appeared at its target: it fails now.
    std::os::unix::fs::symlink(root.join("later.wasm"), base.join("dangling.wasm")).unwrap();
    let err = Resolver::new()
        .base(&base)
        .fetch(&Location::File("dangling.wasm".into()))
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Read { .. })), "{err:?}");
}

#[tokio::test]
async fn a_malformed_reference_fails_even_when_its_artifact_is_cached() {
    let (host, _registry, [bytes, _, _]) = echo_registry().await;
    let cache = scratch("cached-bad-reference");
    let resolver = resolver(&host).cache(&cache);
    let good: Location = format!("oci://{host}/test/echo@{bytes}").parse().unwrap();
    resolver.fetch(&good).await.unwrap();
    let bad: Location = format!("oci://Not A Reference@{bytes}").parse().unwrap();
    let err = resolver.fetch(&bad).await.err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}

/// With no writer, opening a FIFO never returns: it is refused before that.
#[cfg(unix)]
#[tokio::test]
async fn a_fifo_is_not_a_component() {
    let dir = scratch("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .unwrap();
    assert!(made.success());
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Resolver::new()
            .base(&dir)
            .fetch(&Location::File("pipe".into())),
    )
    .await
    .expect("does not wait for a writer");
    assert!(
        matches!(&fetched, Err(SourceError::Read { source, .. }) if source.to_string().contains("not a regular file")),
        "{:?}",
        fetched.err()
    );
}

/// A link swapped in between the check and the open: whichever way each fetch
/// lands, what is outside the base is never read.
#[cfg(unix)]
#[tokio::test]
async fn a_link_swapped_in_after_the_check_is_not_followed() {
    use std::sync::atomic::AtomicBool;
    let dir = scratch("swap");
    let base = dir.join("base");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(dir.join("secret"), b"secret").unwrap();
    std::fs::write(base.join("real"), b"real").unwrap();
    std::os::unix::fs::symlink("../secret", base.join("link")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (stop, base) = (stop.clone(), base.clone());
        std::thread::spawn(move || {
            let (c, held) = (base.join("c.wasm"), base.join("held"));
            std::fs::hard_link(base.join("real"), &c).unwrap();
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&c, &held).unwrap();
                std::fs::rename(base.join("link"), &c).unwrap();
                std::fs::rename(&c, base.join("link")).unwrap();
                std::fs::rename(&held, &c).unwrap();
            }
        })
    };
    let resolver = Resolver::new().base(&base);
    let location = Location::File("c.wasm".into());
    let (mut read, mut refused) = (0, 0);
    for _ in 0..2_000 {
        match resolver.fetch(&location).await {
            Ok(fetched) => {
                assert_eq!(&*fetched.bytes, b"real", "read from outside the base");
                read += 1;
            }
            Err(_) => refused += 1,
        }
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(read + refused == 2_000);
}
