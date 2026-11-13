//! Nodes as OCI artifacts: pushed from their files, and pulled.
#![cfg(test)]
#![allow(missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

use common::*;
use test_components::ECHO;
use witgraph_ir::{ContractId, ContractSource, Graph};
use witgraph_runtime::{Perf, RunningGraph, RuntimeConfig, RuntimeGraph, TickResult, Val};
use witgraph_source::oci::{
    NODE_ARTIFACT_TYPE, NODE_CONFIG_MEDIA_TYPE, PROFILE_ANNOTATION, WASM_CONFIG_MEDIA_TYPE,
    WASM_LAYER_MEDIA_TYPE,
};
use witgraph_source::{Digest, Profile, SourceError};

#[tokio::test]
async fn a_pushed_node_is_one_artifact_its_builds_standard_wasm_artifacts() {
    let (host, registry, digest) = echo_registry(true).await;
    let node = registry.manifest(&format!("test/echo:{digest}"));
    assert_eq!(node["artifactType"], NODE_ARTIFACT_TYPE);
    assert_eq!(node["config"]["mediaType"], NODE_CONFIG_MEDIA_TYPE);
    let layers = node["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "its WIT");
    assert_eq!(layers[0]["mediaType"], WASM_LAYER_MEDIA_TYPE);
    assert_eq!(registry.manifest("test/echo:1.0"), node, "tagged");

    // The subject: an index of the builds, each annotated with its profile.
    let index = registry.manifest(&format!(
        "test/echo:{}",
        node["subject"]["digest"].as_str().unwrap()
    ));
    let entries = index["manifests"].as_array().unwrap();
    let profiles: Vec<&str> = entries
        .iter()
        .map(|e| e["annotations"][PROFILE_ANNOTATION].as_str().unwrap())
        .collect();
    assert_eq!(profiles, ["release", "debug"]);
    for entry in entries {
        // A build is a component as wkg pushes one.
        let build = registry.manifest(&format!("test/echo:{}", entry["digest"].as_str().unwrap()));
        assert_eq!(build["config"]["mediaType"], WASM_CONFIG_MEDIA_TYPE);
        assert_eq!(build["layers"].as_array().unwrap().len(), 1);
        assert_eq!(build["layers"][0]["mediaType"], WASM_LAYER_MEDIA_TYPE);
        assert!(build.get("subject").is_none());
    }

    // Pushed again from the same files, it is the same artifact.
    let dir = project_with_profiles("push-again");
    let again = resolver(&host)
        .base(&dir)
        .push(
            Path::new("echo.node.json"),
            &format!("oci://{host}/test/echo:1.1"),
        )
        .await
        .unwrap();
    assert_eq!(again.digest, digest);
}

#[tokio::test]
async fn a_node_is_pulled_with_the_build_of_its_profile() {
    let (host, _registry, digest) = echo_registry(true).await;
    let location = format!("oci://{host}/test/echo:1.0");
    let release = resolver(&host).component(&echo(), &location).await.unwrap();
    assert_eq!(release.profile, Some(Profile::Release));
    assert_eq!(*release.bytes, *ECHO.wasm);
    assert_eq!(release.artifact, Some(digest));
    let id: ContractId = format!("echo/{ECHO_WORLD}").parse().unwrap();
    assert_eq!(release.contracts[0].id, id);
    assert_eq!(
        release.contracts[0].outputs[0].viewer.as_deref(),
        Some("series")
    );

    let debug = resolver(&host)
        .profile(Profile::Debug)
        .component(&echo(), &location)
        .await
        .unwrap();
    assert_eq!(debug.profile, Some(Profile::Debug));
    assert_eq!(*debug.bytes, *debug_build());
    assert_eq!(debug.contracts, release.contracts);
}

#[tokio::test]
async fn a_node_of_one_build_has_it_used_whatever_the_profile() {
    let (host, registry, digest) = echo_registry(false).await;
    let node = registry.manifest(&format!("test/echo:{digest}"));
    let index = registry.manifest(&format!(
        "test/echo:{}",
        node["subject"]["digest"].as_str().unwrap()
    ));
    assert!(index["manifests"][0].get("annotations").is_none());
    let debug = resolver(&host)
        .profile(Profile::Debug)
        .component(&echo(), &format!("oci://{host}/test/echo:1.0"))
        .await
        .unwrap();
    assert_eq!(debug.profile, None);
    assert_eq!(*debug.bytes, *ECHO.wasm);
}

#[tokio::test]
async fn a_profile_with_no_build_fails_naming_those_there_are() {
    let (host, _registry) = Registry::serve().await;
    let dir = project_with_profiles("release-only-push");
    std::fs::write(
        dir.join("echo.node.json"),
        manifest(
            serde_json::json!({ "release": "echo.wasm" }),
            "wit",
            &[ECHO_WORLD],
        ),
    )
    .unwrap();
    resolver(&host)
        .base(&dir)
        .push(
            Path::new("echo.node.json"),
            &format!("oci://{host}/test/echo:1.0"),
        )
        .await
        .unwrap();
    let err = resolver(&host)
        .profile(Profile::Debug)
        .component(&echo(), &format!("oci://{host}/test/echo:1.0"))
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::NoBuild { profile: Profile::Debug, found, .. }) if found == &[Profile::Release]),
        "{err:?}"
    );
    assert!(
        err.unwrap()
            .to_string()
            .contains("no debug build: it has release")
    );
}

#[tokio::test]
async fn a_node_that_does_not_check_is_not_pushed() {
    let (host, registry) = Registry::serve().await;
    let dir = project("bad-push", test_components::RELAY.wasm, &[ECHO_WORLD]);
    let err = resolver(&host)
        .base(&dir)
        .push(
            Path::new("echo.node.json"),
            &format!("oci://{host}/test/echo:1.0"),
        )
        .await
        .err();
    assert!(matches!(err, Some(SourceError::Verify(_))), "{err:?}");
    assert_eq!(
        registry.requests.load(Ordering::SeqCst),
        0,
        "nothing was pushed"
    );
    let dir = project("digest-push", ECHO.wasm, &[ECHO_WORLD]);
    let pinned = format!("oci://{host}/test/echo@{}", Digest::of(b""));
    let err = resolver(&host)
        .base(&dir)
        .push(Path::new("echo.node.json"), &pinned)
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_pinned_node_in_the_cache_is_not_pulled_again() {
    let (host, registry, digest) = echo_registry(false).await;
    let cache = scratch("pinned-cache");
    let pinned = table(&format!("oci://{host}/test/echo@{digest}"));
    resolver(&host)
        .cache(&cache)
        .resolve(&pinned)
        .await
        .unwrap();
    let pulled = registry.requests.load(Ordering::SeqCst);
    let again = resolver(&host)
        .cache(&cache)
        .resolve(&pinned)
        .await
        .unwrap();
    assert_eq!(
        registry.requests.load(Ordering::SeqCst),
        pulled,
        "all from the cache"
    );
    assert!(!again.get(&echo()).unwrap().offline);

    // A tag may have moved: it is asked of the registry.
    let tagged = table(&format!("oci://{host}/test/echo:1.0"));
    resolver(&host)
        .cache(&cache)
        .resolve(&tagged)
        .await
        .unwrap();
    assert!(registry.requests.load(Ordering::SeqCst) > pulled);
}

#[tokio::test]
async fn a_tag_whose_registry_is_gone_is_the_node_it_held_last_marked_offline() {
    let (host, registry, digest) = echo_registry(false).await;
    let cache = scratch("offline-tag");
    let tagged = table(&format!("oci://{host}/test/echo:1.0"));
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
    assert_eq!(offline.artifact, Some(digest));
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
async fn what_a_registry_serves_is_held_to_its_digest() {
    let (host, registry, _) = echo_registry(false).await;
    let digest = Digest::of(ECHO.wasm).to_string();
    registry.tamper(&digest, test_components::RELAY.wasm);
    let err = resolver(&host)
        .component(&echo(), &format!("oci://{host}/test/echo:1.0"))
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::Pull { message, .. }) if message.contains("digest")),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_artifact_that_is_not_a_node_is_refused() {
    let (host, registry) = Registry::serve().await;
    // A component as wkg pushes one: a build, not a node.
    let config = registry.put_blob(b"{}");
    let layer = registry.put_blob(ECHO.wasm);
    registry.put_manifest(
        "test/echo",
        "1.0",
        &serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "mediaType": WASM_CONFIG_MEDIA_TYPE, "digest": config, "size": 2 },
            "layers": [{ "mediaType": WASM_LAYER_MEDIA_TYPE, "digest": layer, "size": ECHO.wasm.len() }],
        }),
    );
    let err = resolver(&host)
        .component(&echo(), &format!("oci://{host}/test/echo:1.0"))
        .await
        .err();
    assert!(matches!(err, Some(SourceError::NotANode { .. })), "{err:?}");

    // A node of two WIT layers: which is its WIT?
    let description = registry.put_blob(br#"{"wit":"test:echo@0.1.0","worlds":[]}"#);
    let wit = registry.put_blob(&witgraph_wit::load::encode(&ECHO.wit_source()).unwrap());
    let wasm = |digest: &str| serde_json::json!({ "mediaType": WASM_LAYER_MEDIA_TYPE, "digest": digest, "size": 1 });
    registry.put_manifest(
        "test/echo",
        "2.0",
        &serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": NODE_ARTIFACT_TYPE,
            "config": { "mediaType": NODE_CONFIG_MEDIA_TYPE, "digest": description, "size": 38 },
            "layers": [wasm(&wit), wasm(&wit)],
        }),
    );
    let err = resolver(&host)
        .component(&echo(), &format!("oci://{host}/test/echo:2.0"))
        .await
        .err();
    assert!(
        matches!(&err, Some(SourceError::Artifact { message, .. }) if message.contains("several")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_build_over_the_size_limit_is_refused() {
    let (host, _registry, _) = echo_registry(false).await;
    let location = format!("oci://{host}/test/echo:1.0");
    let err = resolver(&host)
        .max_component_bytes(16)
        .component(&echo(), &location)
        .await
        .err();
    assert!(matches!(err, Some(SourceError::TooLarge { .. })), "{err:?}");
}

#[tokio::test]
async fn a_malformed_reference_fails_even_when_its_node_is_cached() {
    let (host, _registry, digest) = echo_registry(false).await;
    let cache = scratch("cached-bad-reference");
    let resolver = resolver(&host).cache(&cache);
    resolver
        .component(&echo(), &format!("oci://{host}/test/echo@{digest}"))
        .await
        .unwrap();
    let err = resolver
        .component(&echo(), &format!("oci://Not A Reference@{digest}"))
        .await
        .err();
    assert!(
        matches!(err, Some(SourceError::Reference { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_cache_that_cannot_be_written_does_not_fail_the_resolve() {
    let (host, _registry, _) = echo_registry(false).await;
    // A file where the cache directory should be: nothing can be kept.
    let blocked = scratch("blocked-cache").join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let components = resolver(&host)
        .cache(&blocked)
        .resolve(&table(&format!("oci://{host}/test/echo:1.0")))
        .await
        .expect("the node was pulled and checked");
    assert_eq!(*components.get(&echo()).unwrap().bytes, *ECHO.wasm);
}

#[tokio::test]
async fn components_sharing_a_node_resolve_alike() {
    let (host, _registry, _) = echo_registry(false).await;
    let location = format!("oci://{host}/test/echo:1.0");
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
async fn a_graph_loads_from_pulled_nodes() {
    let (host, _registry, _) = echo_registry(false).await;
    let table = table(&format!("oci://{host}/test/echo:1.0"));
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
