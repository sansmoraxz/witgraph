//! What the resolve tests share: a project of the `echo` fixture, and an OCI
//! registry in memory that takes pushes.
#![allow(dead_code, missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use test_components::ECHO;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use witgraph_ir::ComponentName;
use witgraph_source::{Digest, Resolver};

/// A fresh directory under the test target directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn echo() -> ComponentName {
    "echo".parse().unwrap()
}

pub const ECHO_WORLD: &str = "test:echo/echo@0.1.0";

/// A node manifest for `echo`: its builds (`component`) and WIT where given.
pub fn manifest(component: serde_json::Value, wit: &str, worlds: &[&str]) -> String {
    serde_json::json!({
        "wit": "test:echo@0.1.0",
        "wit_source": wit,
        "worlds": worlds,
        "component": component,
        "viewers": { ECHO_WORLD: { "out": "series" } },
    })
    .to_string()
}

pub fn copy_dir(from: &Path, to: &Path) {
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

/// A project holding `echo`: its build (`bytes`, as `echo.wasm`), WIT and
/// node manifest (`echo.node.json`).
pub fn project(name: &str, bytes: &[u8], worlds: &[&str]) -> PathBuf {
    let dir = scratch(name);
    std::fs::write(dir.join("echo.wasm"), bytes).unwrap();
    copy_dir(Path::new(ECHO.wit), &dir.join("wit"));
    std::fs::write(
        dir.join("echo.node.json"),
        manifest("echo.wasm".into(), "wit", worlds),
    )
    .unwrap();
    dir
}

/// A project holding `echo` with a build per profile: `echo.wasm` for
/// release and `echo.debug.wasm` (the same bytes, with a custom section, so
/// another digest) for debug.
pub fn project_with_profiles(name: &str) -> PathBuf {
    let dir = project(name, ECHO.wasm, &[ECHO_WORLD]);
    std::fs::write(dir.join("echo.debug.wasm"), debug_build()).unwrap();
    std::fs::write(
        dir.join("echo.node.json"),
        manifest(
            serde_json::json!({ "release": "echo.wasm", "debug": "echo.debug.wasm" }),
            "wit",
            &[ECHO_WORLD],
        ),
    )
    .unwrap();
    dir
}

/// `echo`, told apart from its release build by a custom section.
pub fn debug_build() -> Vec<u8> {
    let mut bytes = ECHO.wasm.to_vec();
    let name = b"debug";
    let payload = [&[name.len() as u8][..], name, b"yes"].concat();
    bytes.push(0);
    bytes.push(payload.len() as u8);
    bytes.extend_from_slice(&payload);
    bytes
}

pub fn table(location: &str) -> BTreeMap<ComponentName, String> {
    BTreeMap::from([(echo(), location.to_string())])
}

pub fn resolver(host: &str) -> Resolver {
    Resolver::new().insecure_registries(vec![host.to_string()])
}

/// An OCI registry holding what is pushed to it, in memory, counting the
/// requests it serves.
#[derive(Default)]
pub struct Registry {
    pub state: Mutex<State>,
    pub requests: AtomicUsize,
    /// Stops it listening: a registry that cannot be reached.
    pub stop: tokio::sync::Notify,
}

#[derive(Default)]
pub struct State {
    /// Manifests, with their media type, by `repository:reference` (a tag or
    /// a digest).
    pub manifests: HashMap<String, (String, Vec<u8>)>,
    /// Blobs by digest (`sha256:…`).
    pub blobs: HashMap<String, Vec<u8>>,
    /// Uploads under way, by id.
    uploads: HashMap<String, Vec<u8>>,
}

impl Registry {
    /// Serves a registry, returning `host:port`.
    pub async fn serve() -> (String, Arc<Registry>) {
        let registry = Arc::new(Self::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                    let Some((method, path, body)) = request(&mut socket).await else {
                        return;
                    };
                    registry.requests.fetch_add(1, Ordering::SeqCst);
                    let (status, headers, body) = registry.answer(&method, &path, body);
                    let mut head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
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

    /// The answer to a request: status, headers and body.
    fn answer(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (String, Vec<(String, String)>, Vec<u8>) {
        let ok = |status: &str| status.to_string();
        let mut state = self.state.lock().unwrap();
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let digest_param = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("digest="))
            .map(|d| d.replace("%3A", ":"));
        let Some(rest) = path.strip_prefix("/v2/") else {
            return (ok("404 Not Found"), vec![], b"{}".to_vec());
        };
        if rest.is_empty() {
            return (ok("200 OK"), vec![], b"{}".to_vec());
        }
        if let Some((repository, reference)) = rest.split_once("/manifests/") {
            let key = format!("{repository}:{reference}");
            return match method {
                "PUT" => {
                    let digest = Digest::of(&body).to_string();
                    let media_type = serde_json::from_slice::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v["mediaType"].as_str().map(str::to_string))
                        .unwrap_or_default();
                    state.manifests.insert(
                        format!("{repository}:{digest}"),
                        (media_type.clone(), body.clone()),
                    );
                    state.manifests.insert(key, (media_type, body));
                    (
                        ok("201 Created"),
                        vec![
                            ("Docker-Content-Digest".into(), digest.clone()),
                            (
                                "Location".into(),
                                format!("/v2/{repository}/manifests/{digest}"),
                            ),
                        ],
                        vec![],
                    )
                }
                _ => match state.manifests.get(&key) {
                    Some((media_type, bytes)) => (
                        ok("200 OK"),
                        vec![
                            ("Content-Type".into(), media_type.clone()),
                            (
                                "Docker-Content-Digest".into(),
                                Digest::of(bytes).to_string(),
                            ),
                        ],
                        bytes.clone(),
                    ),
                    None => (ok("404 Not Found"), vec![], b"{}".to_vec()),
                },
            };
        }
        if let Some((repository, upload)) = rest.split_once("/blobs/uploads/") {
            let location = |id: &str| format!("/v2/{repository}/blobs/uploads/{id}");
            return match method {
                "POST" => {
                    if let Some(digest) = digest_param {
                        state.blobs.insert(digest.clone(), body);
                        return (
                            ok("201 Created"),
                            vec![(
                                "Location".into(),
                                format!("/v2/{repository}/blobs/{digest}"),
                            )],
                            vec![],
                        );
                    }
                    let id = format!("u{}", state.uploads.len() + 1);
                    state.uploads.insert(id.clone(), body);
                    (
                        ok("202 Accepted"),
                        vec![
                            ("Location".into(), location(&id)),
                            ("Range".into(), "0-0".into()),
                        ],
                        vec![],
                    )
                }
                "PATCH" => {
                    let held = state.uploads.entry(upload.to_string()).or_default();
                    held.extend_from_slice(&body);
                    let end = held.len().saturating_sub(1);
                    (
                        ok("202 Accepted"),
                        vec![
                            ("Location".into(), location(upload)),
                            ("Range".into(), format!("0-{end}")),
                        ],
                        vec![],
                    )
                }
                "PUT" => {
                    let mut held = state.uploads.remove(upload).unwrap_or_default();
                    held.extend_from_slice(&body);
                    let digest = digest_param.unwrap_or_default();
                    if Digest::of(&held).to_string() != digest {
                        return (ok("400 Bad Request"), vec![], b"{}".to_vec());
                    }
                    state.blobs.insert(digest.clone(), held);
                    (
                        ok("201 Created"),
                        vec![(
                            "Location".into(),
                            format!("/v2/{repository}/blobs/{digest}"),
                        )],
                        vec![],
                    )
                }
                _ => (ok("405 Method Not Allowed"), vec![], vec![]),
            };
        }
        if let Some((_, digest)) = rest.split_once("/blobs/") {
            return match state.blobs.get(digest) {
                Some(bytes) => (
                    ok("200 OK"),
                    vec![
                        ("Content-Type".into(), "application/octet-stream".into()),
                        ("Docker-Content-Digest".into(), digest.to_string()),
                    ],
                    bytes.clone(),
                ),
                None => (ok("404 Not Found"), vec![], b"{}".to_vec()),
            };
        }
        (ok("404 Not Found"), vec![], b"{}".to_vec())
    }

    /// Replaces what the blob `digest` holds: a registry serving what its
    /// digest does not name.
    pub fn tamper(&self, digest: &str, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(digest.to_string(), bytes.to_vec());
    }

    /// The manifest at `repository:reference`, as JSON.
    pub fn manifest(&self, key: &str) -> serde_json::Value {
        let state = self.state.lock().unwrap();
        serde_json::from_slice(&state.manifests[key].1).unwrap()
    }

    /// Stores `manifest` (JSON) at `repository:reference` and by its digest,
    /// as a push would; returns its digest.
    pub fn put_manifest(
        &self,
        repository: &str,
        reference: &str,
        manifest: &serde_json::Value,
    ) -> String {
        let bytes = serde_json::to_vec(manifest).unwrap();
        let (status, headers, _) = self.answer(
            "PUT",
            &format!("/v2/{repository}/manifests/{reference}"),
            bytes,
        );
        assert!(status.starts_with("201"));
        headers
            .into_iter()
            .find(|(name, _)| name == "Docker-Content-Digest")
            .unwrap()
            .1
    }

    /// Stores a blob; returns its digest.
    pub fn put_blob(&self, bytes: &[u8]) -> String {
        let digest = Digest::of(bytes).to_string();
        self.tamper(&digest, bytes);
        digest
    }
}

/// A request's method, path and body.
async fn request(socket: &mut tokio::net::TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 8192];
    let end = loop {
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
        }
    };
    let head = String::from_utf8_lossy(&request[..end]).into_owned();
    let mut lines = head.lines();
    let mut words = lines.next()?.split_whitespace();
    let (method, path) = (words.next()?.to_string(), words.next()?.to_string());
    let length: usize = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = request[end..].to_vec();
    while body.len() < length {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&buffer[..n]),
        }
    }
    Some((method, path, body))
}

/// A registry holding `echo` pushed from a project, with a build per
/// profile when `profiles`, as `test/echo:1.0`. Returns the host, the
/// registry, and the node's digest.
pub async fn echo_registry(profiles: bool) -> (String, Arc<Registry>, Digest) {
    let (host, registry) = Registry::serve().await;
    let dir = if profiles {
        project_with_profiles(&format!("pushed-{}", host.replace([':', '.'], "-")))
    } else {
        project(
            &format!("pushed-{}", host.replace([':', '.'], "-")),
            ECHO.wasm,
            &[ECHO_WORLD],
        )
    };
    let pushed = resolver(&host)
        .base(&dir)
        .push(
            Path::new("echo.node.json"),
            &format!("oci://{host}/test/echo:1.0"),
        )
        .await
        .unwrap();
    (host, registry, pushed.digest)
}
