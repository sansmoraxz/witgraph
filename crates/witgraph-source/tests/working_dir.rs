//! Confinement with the default base, the working directory. A test binary
//! of its own: it changes the process's working directory.
#![cfg(unix)]
#![allow(missing_docs)]

use std::path::PathBuf;

use witgraph_source::{Location, Resolver, SourceError};

#[tokio::test]
async fn a_symbolic_link_out_of_the_working_directory_is_refused() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("working-dir");
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(root.join("secret.wasm"), b"secret").unwrap();
    std::fs::write(project.join("inside.wasm"), b"inside").unwrap();
    std::os::unix::fs::symlink(root.join("secret.wasm"), project.join("evil.wasm")).unwrap();
    std::env::set_current_dir(&project).unwrap();

    // No base: relative paths are the working directory's.
    let resolver = Resolver::new();
    let evil = Location::File("evil.wasm".into());
    let err = resolver.fetch(&evil).await.err();
    assert!(
        matches!(err, Some(SourceError::OutsideBase { .. })),
        "{err:?}"
    );
    let inside = Location::File("inside.wasm".into());
    let inside = resolver
        .fetch(&inside)
        .await
        .expect("a file inside is read");
    assert_eq!(&*inside.bytes, b"inside");
}
