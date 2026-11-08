use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const GUESTS: [&str; 53] = [
    "echo",
    "m0-import",
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
    "edge-detect-webgpu",
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
    "corpus-relay",
    "corpus-flat",
    "line-walk",
    "line-stream",
    "line-stream-sink",
    "c-walk",
    "zig-walk",
    "generate",
];

/// The guests built with debug info and no optimizing, so that each source line is there
/// to step to.
const WITH_DEBUG_INFO: [&str; 5] = [
    "line-walk",
    "line-stream",
    "line-stream-sink",
    "c-walk",
    "zig-walk",
];

/// The guests written in C, built with clang (`CLANG`, or `clang` on the path) and its
/// `wasm-ld`: `src/*.c` is the guest's own code, `bindings/` what `wit-bindgen c` makes of its
/// WIT, and `support/` the little of a C library the bindings use.
const C_GUESTS: [&str; 1] = ["c-walk"];

/// The guests written in Zig, built with `zig` (`ZIG`, or `zig` on the path), each with the
/// file its module starts at: `src/*.zig` is the guest's own code, which calls `bindings/`, what
/// `wit-bindgen c` makes of its WIT, as C. Those and `support/` are built as a C guest's are.
const ZIG_GUESTS: [(&str, &str); 1] = [("zig-walk", "src/walk.zig")];

/// Names guests, comma-separated, to build with debug info too, as `{name}.debug.wasm`.
const DEBUG_GUESTS: &str = "WITGRAPH_DEBUG_GUESTS";

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

    // What a demo asks for as well, of each guest: a build with debug info, which takes a
    // while and is big, so nothing else pays for it.
    println!("cargo::rerun-if-env-changed={DEBUG_GUESTS}");
    let debug_guests = env::var(DEBUG_GUESTS).unwrap_or_default();
    let debug_guests: Vec<&str> = debug_guests
        .split(',')
        .filter(|name| !name.is_empty())
        .collect();
    let debug_dir = target_dir.join("with-debug-info");

    for name in GUESTS {
        // Directories are scanned recursively, so `wit/deps` is covered.
        println!("cargo::rerun-if-changed=guests/{name}/src");
        println!("cargo::rerun-if-changed=guests/{name}/wit");

        let debug = WITH_DEBUG_INFO.contains(&name);
        let component = if C_GUESTS.contains(&name) {
            println!("cargo::rerun-if-changed=guests/{name}/bindings");
            println!("cargo::rerun-if-changed=guests/{name}/support");
            println!("cargo::rerun-if-changed=guests/c-lib");
            println!("cargo::rerun-if-env-changed=CLANG");
            build_c_guest(name, &guest_dir, &out_dir)
        } else if let Some((_, root)) = ZIG_GUESTS.iter().find(|(zig, _)| *zig == name) {
            println!("cargo::rerun-if-changed=guests/{name}/bindings");
            println!("cargo::rerun-if-changed=guests/{name}/support");
            println!("cargo::rerun-if-env-changed=CLANG");
            println!("cargo::rerun-if-env-changed=ZIG");
            build_zig_guest(name, root, &guest_dir, &out_dir)
        } else {
            // Only a Rust guest has one: Cargo takes a missing file as changed, every build.
            println!("cargo::rerun-if-changed=guests/{name}/Cargo.toml");
            build_guest(name, &guest_dir, &target_dir, debug)
        };
        let wasm_dst = out_dir.join(format!("{name}.wasm"));
        fs::write(&wasm_dst, &component)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", wasm_dst.display()));

        if debug_guests.contains(&name) {
            let component = if debug {
                component
            } else {
                build_guest(name, &guest_dir, &debug_dir, true)
            };
            let debug_dst = out_dir.join(format!("{name}.debug.wasm"));
            fs::write(&debug_dst, &component)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", debug_dst.display()));
        }

        println!(
            "cargo::rustc-env={}_WASM={}",
            name.replace('-', "_").to_uppercase(),
            wasm_dst.display()
        );
    }
}

/// Builds a guest in `target_dir`, with debug info and no optimizing if `debug`, as a component.
fn build_guest(name: &str, guest_dir: &Path, target_dir: &Path, debug: bool) -> Vec<u8> {
    let crate_dir = guest_dir.join(name);
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        // Pinned, because the wasm is read from there below: an inherited
        // `CARGO_TARGET_DIR` would send the build elsewhere.
        .arg("--target-dir")
        .arg(target_dir)
        // The outer build's flags target the host, not the guest.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // Plain rustc: a caching wrapper may record the guest's sources under another path
        // (kache gives `/proc/self/cwd`), and the line debugging tests read them from its DWARF.
        // An empty value overrides a `build.rustc-wrapper` in Cargo config too.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .envs(
            debug
                .then_some([
                    ("CARGO_PROFILE_RELEASE_DEBUG", "true"),
                    ("CARGO_PROFILE_RELEASE_STRIP", "false"),
                    ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "0"),
                    ("CARGO_PROFILE_RELEASE_LTO", "false"),
                ])
                .into_iter()
                .flatten(),
        )
        .status()
        .unwrap_or_else(|e| panic!("failed to build {name} guest: {e}"));
    assert!(status.success(), "cargo build failed for {name} guest");

    let lib_name = format!("{}_guest", name.replace('-', "_"));
    let core_wasm = target_dir
        .join("wasm32-unknown-unknown/release")
        .join(format!("{lib_name}.wasm"));
    let module = fs::read(&core_wasm)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", core_wasm.display()));
    componentize(name, &crate_dir, &module)
}

/// Builds a guest written in C as a component, its own code with debug info and no
/// optimizing, so that each line is there to step to; its bindings and support without, so a
/// debugger stops only in its own code.
fn build_c_guest(name: &str, guest_dir: &Path, out_dir: &Path) -> Vec<u8> {
    let dir = guest_dir.join(name);
    let work = out_dir.join("c").join(name);
    fs::create_dir_all(&work)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", work.display()));
    // `c-lib` is a library kept outside the guest's directory, as one is.
    let objects = c_objects(
        name,
        &dir,
        &work,
        &[guest_dir.join("c-lib/include")],
        &[
            ("src", &["-g", "-O0"]),
            ("bindings", &["-g0", "-O2"]),
            ("support", &["-g0", "-O2"]),
        ],
    );
    link_guest(name, &dir, &work, objects)
}

/// Builds a guest written in Zig: its own code from `root` on, with debug info and no
/// optimizing, and its C bindings and support without, as a C guest's are.
fn build_zig_guest(name: &str, root: &str, guest_dir: &Path, out_dir: &Path) -> Vec<u8> {
    let dir = guest_dir.join(name);
    let work = out_dir.join("zig").join(name);
    fs::create_dir_all(&work)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", work.display()));
    let header = fs::read_dir(dir.join("bindings"))
        .unwrap_or_else(|e| panic!("failed to read the bindings of {name}: {e}"))
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "h"))
        .unwrap_or_else(|| panic!("{name} has no bindings header"));
    // The header as Zig has it, which the guest imports as `c`: made here, so that what is
    // generated is no file of the guest's project.
    let translated = work.join("c.zig");
    let header_zig = zig(name, &dir, &work, |command| {
        command
            .args(["translate-c", "-target", "wasm32-freestanding"])
            .arg(&header);
    });
    fs::write(&translated, header_zig)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", translated.display()));
    let own = work.join("own.o");
    zig(name, &dir, &work, |command| {
        command
            .args([
                "build-obj",
                "-target",
                "wasm32-freestanding",
                "-O",
                "Debug",
                "-fllvm",
            ])
            .args(["--dep", "c"])
            .arg(format!("-Mroot={root}"))
            .arg(format!("-Mc={}", translated.display()))
            .arg(format!("-femit-bin={}", own.display()));
    });
    let mut objects = vec![own];
    objects.extend(c_objects(
        name,
        &dir,
        &work,
        &[],
        &[("bindings", &["-g0", "-O2"]), ("support", &["-g0", "-O2"])],
    ));
    link_guest(name, &dir, &work, objects)
}

/// Runs `zig` as `args` say, in the guest's directory, with its cache in `work`. Says what
/// it wrote to its output.
fn zig(name: &str, dir: &Path, work: &Path, args: impl FnOnce(&mut Command)) -> Vec<u8> {
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let mut command = Command::new(&zig);
    command.current_dir(dir);
    args(&mut command);
    command.arg("--cache-dir").arg(work.join("zig-cache"));
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("failed to run {zig} for the {name} guest: {e}"));
    if !output.status.success() {
        panic!(
            "{zig} failed for the {name} guest:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.stdout
}

/// Compiles the C files of each of `parts`, a subdirectory of `dir` and its flags, into
/// objects in `work`.
fn c_objects(
    name: &str,
    dir: &Path,
    work: &Path,
    more_includes: &[PathBuf],
    parts: &[(&str, &[&str])],
) -> Vec<PathBuf> {
    let mut includes = vec![
        OsString::from("-I"),
        dir.join("bindings").into(),
        OsString::from("-I"),
        dir.join("support/include").into(),
    ];
    for include in more_includes {
        includes.extend([OsString::from("-I"), include.into()]);
    }
    let mut objects = Vec::new();
    for (sub, flags) in parts {
        for file in c_files(&dir.join(sub)) {
            let object = work.join(file.file_name().unwrap()).with_extension("o");
            let mut args: Vec<OsString> = flags.iter().map(OsString::from).collect();
            args.extend(["-Wall", "-Werror", "-c"].map(OsString::from));
            args.extend(includes.iter().cloned());
            // By its full path, as the debug info names it for a debugger to find.
            args.extend([file.into(), "-o".into(), object.clone().into()]);
            clang(name, dir, args);
            objects.push(object);
        }
    }
    objects
}

/// Links `objects` into a core module, with the world of the guest in `dir` embedded, and
/// makes a component of it.
fn link_guest(name: &str, dir: &Path, work: &Path, objects: Vec<PathBuf>) -> Vec<u8> {
    let core_wasm = work.join("core.wasm");
    let mut args = vec![OsString::from("-Wl,--no-entry")];
    args.extend(objects.into_iter().map(OsString::from));
    args.extend(["-o".into(), core_wasm.clone().into()]);
    clang(name, dir, args);

    let mut module = fs::read(&core_wasm)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", core_wasm.display()));
    // What `wit-bindgen` embeds for a Rust guest, its world, embedded here.
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve
        .push_dir(dir.join("wit"))
        .unwrap_or_else(|e| panic!("failed to read the WIT of {name}: {e:#}"));
    let world = resolve
        .select_world(&[package], None)
        .unwrap_or_else(|e| panic!("{name} has no world: {e:#}"));
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        // As the bindings name what they import and export: not by the canonical names.
        false,
    )
    .unwrap_or_else(|e| panic!("failed to embed the world of {name}: {e:#}"));
    componentize(name, dir, &module)
}

/// The C files in `dir`, in order.
fn c_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    files.sort();
    files
}

/// Runs clang for wasm with no C library, for the guest `name`, in its directory `dir`, as a
/// project's build runs: the debug info says it was compiled there.
fn clang(name: &str, dir: &Path, args: Vec<OsString>) {
    let clang = env::var("CLANG").unwrap_or_else(|_| "clang".into());
    let status = Command::new(&clang)
        .current_dir(dir)
        .args([
            "--target=wasm32-unknown-unknown",
            "-nostdlib",
            "-ffreestanding",
        ])
        .args(args)
        .status()
        .unwrap_or_else(|e| {
            panic!("failed to run {clang} for the {name} guest, which needs clang and wasm-ld: {e}")
        });
    assert!(status.success(), "{clang} failed for the {name} guest");
}

/// A guest's core module as a component, with its world's docs.
fn componentize(name: &str, crate_dir: &Path, module: &[u8]) -> Vec<u8> {
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(module)
        .unwrap_or_else(|e| panic!("failed to encode {name} module: {e:#}"))
        .encode()
        .unwrap_or_else(|e| panic!("failed to produce {name} component: {e:#}"));

    // A component's bytes keep no doc comments: the world's go in the
    // `witgraph-docs` section, for an editor to describe the node by.
    let wit = fs::read_to_string(crate_dir.join("wit/world.wit"))
        .unwrap_or_else(|e| panic!("failed to read the WIT of {name}: {e}"));
    match world_docs(&wit) {
        Some(docs) => with_section(component, "witgraph-docs", docs.as_bytes()),
        None => component,
    }
}
