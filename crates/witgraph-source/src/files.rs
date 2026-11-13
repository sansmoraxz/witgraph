//! Reading files under the resolver's base: confined to it unless allowed
//! otherwise, refused past the size limit, and digested as they are read.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use crate::{Digest, Location, Resolver, SourceError};

impl Resolver {
    /// The file at `path` (relative to the base), and its digest, read as a
    /// node manifest and its builds are: confined to the base unless
    /// [allowed](Self::allow_paths_outside_base) otherwise, and refused past
    /// [the size limit](Self::max_component_bytes).
    pub async fn read_file(&self, path: &Path) -> Result<(Bytes, Digest), SourceError> {
        let full = self.confined(path).await?;
        let file = open(&full).await?;
        if !self.any_path {
            self.still_confined(path, &full, &file).await?;
        }
        let (bytes, digest) = read_file(file, &full, self.max_bytes)
            .await?
            .ok_or_else(|| SourceError::TooLarge {
                location: Location::File(path.to_path_buf()),
                limit: self.max_bytes,
            })?;
        Ok((bytes.into(), digest))
    }

    /// The path a file is read from: `path` under the
    /// base, refused unless it stays there once its symbolic links are
    /// followed (and then the path they lead to, so a link changed after
    /// this check is not followed). A file that is missing, or a link that
    /// leads nowhere, fails here, as its read would.
    pub(crate) async fn confined(&self, path: &Path) -> Result<PathBuf, SourceError> {
        let full = self.base.join(path);
        if self.any_path {
            return Ok(full);
        }
        let outside = || SourceError::OutsideBase {
            path: path.to_path_buf(),
        };
        let lexically_outside = path.is_absolute()
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
        if lexically_outside {
            return Err(outside());
        }
        // An empty base is the working directory.
        let base = match self.base.as_os_str().is_empty() {
            true => Path::new("."),
            false => &self.base,
        };
        let unreadable = |source| SourceError::Read {
            path: full.clone(),
            source,
        };
        let base = tokio::fs::canonicalize(base).await.map_err(unreadable)?;
        let file = tokio::fs::canonicalize(&full).await.map_err(unreadable)?;
        if !file.starts_with(&base) {
            return Err(outside());
        }
        Ok(file)
    }

    /// That `file`, opened from `full`, is what [`confined`](Self::confined) found:
    /// a link swapped in between that check and the open led it elsewhere. Asked of
    /// the file already open, so what is swapped afterwards changes nothing.
    async fn still_confined(
        &self,
        path: &Path,
        full: &Path,
        file: &tokio::fs::File,
    ) -> Result<(), SourceError> {
        let failed = |source| SourceError::Read {
            path: full.to_path_buf(),
            source,
        };
        let outside = || SourceError::OutsideBase {
            path: path.to_path_buf(),
        };
        if self.confined(path).await? != full {
            return Err(outside());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata().await.map_err(failed)?;
            let there = tokio::fs::symlink_metadata(full).await.map_err(failed)?;
            if (opened.dev(), opened.ino()) != (there.dev(), there.ino()) {
                return Err(outside());
            }
        }
        #[cfg(not(unix))]
        let _ = (file, failed);
        Ok(())
    }
}

/// Whether the file at `path` starts as WebAssembly does.
pub(crate) fn is_wasm(path: &Path) -> bool {
    use std::io::Read as _;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok_and(|()| magic == *b"\0asm")
}

/// A buffer that refuses to grow past `limit` bytes, hashing what it
/// takes as it arrives (so no separate pass over the bytes stalls the
/// other fetches).
pub(crate) struct Capped {
    bytes: Vec<u8>,
    hash: Sha256,
    limit: u64,
    /// Whether a write would have gone past the limit.
    pub(crate) over: bool,
}

impl Capped {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            hash: Sha256::new(),
            limit,
            over: false,
        }
    }

    /// Takes `buf`, unless that would go past the limit.
    pub(crate) fn push(&mut self, buf: &[u8]) -> bool {
        if (self.bytes.len() + buf.len()) as u64 > self.limit {
            self.over = true;
            return false;
        }
        self.bytes.extend_from_slice(buf);
        self.hash.update(buf);
        true
    }

    /// The bytes taken, and their digest.
    pub(crate) fn finish(self) -> (Vec<u8>, Digest) {
        (self.bytes, Digest(hex::encode(self.hash.finalize())))
    }
}

impl tokio::io::AsyncWrite for Capped {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if !self.get_mut().push(buf) {
            return std::task::Poll::Ready(Err(std::io::Error::other("over the size limit")));
        }
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// The file at `path` and its digest, or `None` when it is over `limit`
/// bytes.
pub(crate) async fn read(
    path: &Path,
    limit: u64,
) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
    read_file(open(path).await?, path, limit).await
}

/// `path`, which must be a regular file: without a writer, opening a FIFO would
/// wait forever, and neither the size limit nor a digest would get to look at it.
pub(crate) async fn open(path: &Path) -> Result<tokio::fs::File, SourceError> {
    let failed = |source| SourceError::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).await.map_err(failed)?;
    if !file.metadata().await.map_err(failed)?.is_file() {
        return Err(failed(std::io::Error::other("not a regular file")));
    }
    Ok(file)
}

/// The rest of `file`, read from `path`, and its digest, or `None` when it is over
/// `limit` bytes.
async fn read_file(
    mut file: tokio::fs::File,
    path: &Path,
    limit: u64,
) -> Result<Option<(Vec<u8>, Digest)>, SourceError> {
    use tokio::io::AsyncReadExt;
    let failed = |source| SourceError::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut out = Capped::new(limit);
    let mut chunk = vec![0; 64 << 10];
    loop {
        // Checked as it is read, so a file that grows is still cut off.
        match file.read(&mut chunk).await.map_err(failed)? {
            0 => return Ok(Some(out.finish())),
            n if !out.push(&chunk[..n]) => return Ok(None),
            _ => {}
        }
    }
}
