use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let guest_dir = manifest_dir.join("guests");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    for name in ["echo", "configurable", "stream-producer", "stream-consumer", "mqtt-node"] {
        let crate_dir = guest_dir.join(name);

        println!("cargo::rerun-if-changed=guests/{name}/src/lib.rs");
        println!("cargo::rerun-if-changed=guests/{name}/Cargo.toml");

        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--manifest-path",
            ])
            .arg(crate_dir.join("Cargo.toml").to_str().unwrap())
            .status()
            .unwrap_or_else(|e| panic!("failed to build {name} guest: {e}"));

        assert!(status.success(), "cargo build failed for {name} guest");

        let lib_name = format!("{}_guest", name.replace('-', "_"));
        let core_wasm = crate_dir
            .join("target/wasm32-unknown-unknown/release")
            .join(format!("{lib_name}.wasm"));

        let module = fs::read(&core_wasm).unwrap_or_else(|e| {
            panic!("failed to read {}: {e}", core_wasm.display())
        });

        let component = wit_component::ComponentEncoder::default()
            .module(&module)
            .unwrap_or_else(|e| panic!("failed to encode {name} module: {e}"))
            .encode()
            .unwrap_or_else(|e| panic!("failed to produce {name} component: {e}"));

        let wasm_dst = out_dir.join(format!("{name}.wasm"));
        fs::write(&wasm_dst, &component).unwrap_or_else(|e| {
            panic!("failed to write {}: {e}", wasm_dst.display())
        });

        println!(
            "cargo::rustc-env={}_WASM={}",
            name.replace('-', "_").to_uppercase(),
            wasm_dst.display()
        );
    }
}
