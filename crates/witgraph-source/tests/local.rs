//! Nodes as files: a node manifest naming its WIT and builds.
#![cfg(test)]
#![allow(missing_docs)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::*;
use test_components::{ECHO, RELAY};
use witgraph_ir::{ContractId, ContractSource};
use witgraph_source::{Digest, Profile, Resolver, SourceError};

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
    assert_eq!(echo_component.profile, None, "its one build");
    assert_eq!(echo_component.artifact, None, "not pulled");
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
async fn a_node_with_a_build_per_profile_resolves_the_one_asked_for() {
    let dir = project_with_profiles("profiles");
    let release = Resolver::new()
        .base(&dir)
        .component(&echo(), "echo.node.json")
        .await
        .unwrap();
    assert_eq!(release.profile, Some(Profile::Release));
    assert_eq!(*release.bytes, *ECHO.wasm);
    let debug = Resolver::new()
        .base(&dir)
        .profile(Profile::Debug)
        .component(&echo(), "echo.node.json")
        .await
        .unwrap();
    assert_eq!(debug.profile, Some(Profile::Debug));
    assert_eq!(*debug.bytes, *debug_build());
    assert_eq!(
        debug.contracts, release.contracts,
        "one description, one contract"
    );

    // One build only: the other profile is not quietly the one there is.
    std::fs::write(
        dir.join("release-only.node.json"),
        manifest(
            serde_json::json!({ "release": "echo.wasm" }),
            "wit",
            &[ECHO_WORLD],
        ),
    )
    .unwrap();
    let err = Resolver::new()
        .base(&dir)
        .profile(Profile::Debug)
        .component(&echo(), "release-only.node.json")
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::NoBuild { profile: Profile::Debug, found, .. }) if found == &[Profile::Release]),
        "{err:?}"
    );
    // A node of one build has it used whatever the profile.
    let one = Resolver::new()
        .base(&dir)
        .profile(Profile::Debug)
        .component(&echo(), "echo.node.json")
        .await;
    assert!(one.is_ok());
}

#[tokio::test]
async fn a_wit_package_built_to_wasm_is_a_wit_source_and_a_component_is_not() {
    // As `wkg build` writes the package: one wasm file.
    let dir = project("encoded-wit", ECHO.wasm, &[ECHO_WORLD]);
    let encoded = witgraph_wit::load::encode(&ECHO.wit_source()).unwrap();
    std::fs::write(dir.join("echo-wit.wasm"), encoded).unwrap();
    std::fs::write(
        dir.join("echo.node.json"),
        manifest("echo.wasm".into(), "echo-wit.wasm", &[ECHO_WORLD]),
    )
    .unwrap();
    let components = Resolver::new()
        .base(&dir)
        .resolve(&table("echo.node.json"))
        .await
        .unwrap();
    let id: ContractId = format!("echo/{ECHO_WORLD}").parse().unwrap();
    assert!(components.contract(&id).is_some());

    // The component's own bytes: its WIT is never read out of it.
    std::fs::write(
        dir.join("echo.node.json"),
        manifest("echo.wasm".into(), "echo.wasm", &[ECHO_WORLD]),
    )
    .unwrap();
    let err = Resolver::new()
        .base(&dir)
        .component(&echo(), "echo.node.json")
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::Wit { message, .. }) if message.contains("not a WIT package")),
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
    let other = manifest("echo.wasm".into(), "wit", &[ECHO_WORLD]).replace(
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
    // Not a manifest, or one naming an OCI artifact: a manifest names files.
    for (name, text) in [
        ("bad", r#"{"wit": 1}"#.to_string()),
        (
            "oci",
            manifest("oci://ghcr.io/demo/echo:1".into(), "wit", &[ECHO_WORLD]),
        ),
    ] {
        std::fs::write(dir.join(format!("{name}.node.json")), text).unwrap();
        let err = Resolver::new()
            .base(&dir)
            .component(&echo(), &format!("{name}.node.json"))
            .await
            .err();
        assert!(
            matches!(err, Some(SourceError::Manifest { .. })),
            "{name}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_component_over_the_size_limit_is_refused() {
    let dir = project("too-large", ECHO.wasm, &[ECHO_WORLD]);
    let small = Resolver::new().base(&dir).max_component_bytes(16);
    let err = small.read_file(Path::new("echo.wasm")).await.err();
    assert!(
        matches!(err, Some(SourceError::TooLarge { limit: 16, .. })),
        "{err:?}"
    );
    let exact = ECHO.wasm.len() as u64;
    Resolver::new()
        .base(&dir)
        .max_component_bytes(exact)
        .read_file(Path::new("echo.wasm"))
        .await
        .expect("a build of exactly the limit is fine");
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
            .read_file(Path::new(path))
            .await
            .err();
        assert!(
            matches!(err, Some(SourceError::OutsideBase { .. })),
            "{path}: {err:?}"
        );
    }
    let trusted = Resolver::new().base(&base).allow_paths_outside_base();
    let (bytes, _) = trusted
        .read_file(Path::new("../secret.wasm"))
        .await
        .expect("a trusted graph may");
    assert_eq!(*bytes, *ECHO.wasm);
    // As a table names a node.
    let err = Resolver::new()
        .base(&base)
        .component(&echo(), "../secret.node.json")
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::OutsideBase { .. })),
        "{err:?}"
    );
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
        .read_file(Path::new("evil.wasm"))
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
        .read_file(Path::new("dangling.wasm"))
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Read { .. })), "{err:?}");
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
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Resolver::new().base(&dir).read_file(Path::new("pipe")),
    )
    .await
    .expect("does not wait for a writer");
    assert!(
        matches!(&read, Err(SourceError::Read { source, .. }) if source.to_string().contains("not a regular file")),
        "{:?}",
        read.err()
    );
}

/// A link swapped in between the check and the open: whichever way each read
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
    let (mut read, mut refused) = (0, 0);
    for _ in 0..2_000 {
        match resolver.read_file(Path::new("c.wasm")).await {
            Ok((bytes, _)) => {
                assert_eq!(&*bytes, b"real", "read from outside the base");
                read += 1;
            }
            Err(_) => refused += 1,
        }
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(read + refused == 2_000);
}
