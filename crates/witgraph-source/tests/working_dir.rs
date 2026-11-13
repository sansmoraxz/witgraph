//! Confinement with the default base, the working directory. A test binary
//! of its own: it changes the process's working directory.
#![cfg(unix)]
#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use witgraph_source::{Resolver, SourceError};

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
    let err = resolver.read_file(Path::new("evil.wasm")).await.err();
    assert!(
        matches!(err, Some(SourceError::OutsideBase { .. })),
        "{err:?}"
    );
    let (inside, _) = resolver
        .read_file(Path::new("inside.wasm"))
        .await
        .expect("a file inside is read");
    assert_eq!(&*inside, b"inside");
}
