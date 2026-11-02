use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

const GUESTS: [&str; 43] = [
    "echo",
    "configurable",
    "relay",
    "stream-producer",
    "stream-consumer",
    "stream-relay",
    "stream-drain",
    "future-writer",
    "future-reader",
    "mqtt-node",
    "busy-loop",
    "maybe",
    "relay-plus",
    "relay-wide",
    "named-echo",
    "labelled",
    "math",
    "calc",
    "reading-producer",
    "reading-consumer",
    "bytes-consumer",
    "video-source",
    "frame-grayscale",
    "frame-invert",
    "video-sink",
    "edge-detect",
    "blob-source",
    "blob-relay",
    "blob-sink",
    "tensor-source",
    "tensor-invert",
    "tensor-copy-invert",
    "tensor-sink",
    "tensor-keep",
    "tensor-edge",
    "future-dropper",
    "tensor-stream-source",
    "tensor-stream-invert",
    "tensor-stream-sink",
    "tensor-stream-edge",
    "camera-source",
    "tensor-future-source",
    "tensor-future-sink",
];

/// The doc comment of the first world in `wit`, as the text WIT gives it.
fn world_docs(wit: &str) -> Option<String> {
    let lines: Vec<&str> = wit.lines().map(str::trim).collect();
    let world = lines.iter().position(|line| line.starts_with("world "))?;
    let mut docs: Vec<&str> = lines[..world]
        .iter()
        .rev()
        .map_while(|line| line.strip_prefix("///"))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect();
    docs.reverse();
    (!docs.is_empty()).then(|| docs.join("\n"))
}

/// `component` with a custom section appended at the end of its root. The
/// way a guest's author would declare its description is a static in the
/// guest, but these are written once, in the WIT.
fn with_section(mut component: Vec<u8>, name: &str, data: &[u8]) -> Vec<u8> {
    fn leb(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    let mut body = Vec::new();
    leb(name.len(), &mut body);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(data);
    component.push(0);
    leb(body.len(), &mut component);
    component.extend(body);
    component
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let guest_dir = manifest_dir.join("guests");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // One target directory for every guest, so the toolchain they share
    // (wit-bindgen and its dependencies) is built once.
    let target_dir = guest_dir.join("target");

    // The guests share one workspace manifest and lockfile (the toolchain
    // versions live there), so changing either rebuilds all of them.
    println!("cargo::rerun-if-changed=guests/Cargo.toml");
    println!("cargo::rerun-if-changed=guests/Cargo.lock");

    for name in GUESTS {
        let crate_dir = guest_dir.join(name);

        // Directories are scanned recursively, so `wit/deps` is covered.
        println!("cargo::rerun-if-changed=guests/{name}/src");
        println!("cargo::rerun-if-changed=guests/{name}/wit");
        println!("cargo::rerun-if-changed=guests/{name}/Cargo.toml");

        let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .arg("--manifest-path")
            .arg(crate_dir.join("Cargo.toml"))
            // Pinned, because the wasm is read from there below: an inherited
            // `CARGO_TARGET_DIR` would send the build elsewhere.
            .arg("--target-dir")
            .arg(&target_dir)
            // The outer build's flags target the host, not the guest.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .status()
            .unwrap_or_else(|e| panic!("failed to build {name} guest: {e}"));
        assert!(status.success(), "cargo build failed for {name} guest");

        let lib_name = format!("{}_guest", name.replace('-', "_"));
        let core_wasm = target_dir
            .join("wasm32-unknown-unknown/release")
            .join(format!("{lib_name}.wasm"));
        let module = fs::read(&core_wasm)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", core_wasm.display()));

        let component = wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&module)
            .unwrap_or_else(|e| panic!("failed to encode {name} module: {e:#}"))
            .encode()
            .unwrap_or_else(|e| panic!("failed to produce {name} component: {e:#}"));

        // A component's bytes keep no doc comments: the world's go in the
        // `witgraph-docs` section, for an editor to describe the node by.
        let wit = fs::read_to_string(crate_dir.join("wit/world.wit"))
            .unwrap_or_else(|e| panic!("failed to read the WIT of {name}: {e}"));
        let component = match world_docs(&wit) {
            Some(docs) => with_section(component, "witgraph-docs", docs.as_bytes()),
            None => component,
        };

        let wasm_dst = out_dir.join(format!("{name}.wasm"));
        fs::write(&wasm_dst, &component)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", wasm_dst.display()));

        println!(
            "cargo::rustc-env={}_WASM={}",
            name.replace('-', "_").to_uppercase(),
            wasm_dst.display()
        );
    }
}
