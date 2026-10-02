use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

const GUESTS: [&str; 7] = [
    "echo",
    "configurable",
    "relay",
    "stream-producer",
    "stream-consumer",
    "mqtt-node",
    "busy-loop",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let guest_dir = manifest_dir.join("guests");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

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
            // The outer build's flags target the host, not the guest.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .status()
            .unwrap_or_else(|e| panic!("failed to build {name} guest: {e}"));
        assert!(status.success(), "cargo build failed for {name} guest");

        let lib_name = format!("{}_guest", name.replace('-', "_"));
        let core_wasm = crate_dir
            .join("target/wasm32-unknown-unknown/release")
            .join(format!("{lib_name}.wasm"));
        let module = fs::read(&core_wasm)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", core_wasm.display()));

        let component = wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&module)
            .unwrap_or_else(|e| panic!("failed to encode {name} module: {e:#}"))
            .encode()
            .unwrap_or_else(|e| panic!("failed to produce {name} component: {e:#}"));

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
