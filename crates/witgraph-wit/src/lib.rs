//! WIT loading and lowering into the witgraph IR.

pub mod hash;
pub mod load;
pub mod lower;
pub mod metadata;

pub use witgraph_ir as ir;

use std::path::Path;

use witgraph_ir::ComponentContract;

/// Failure from [`load_components`]: loading the WIT source or lowering its
/// worlds.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Loading or resolving the WIT source failed.
    #[error(transparent)]
    Load(#[from] load::LoadError),
    /// One or more worlds failed to lower.
    #[error(transparent)]
    Lower(#[from] lower::LowerFailures),
}

/// Load a `.wit` file or directory and lower every witgraph component world
/// found in it, keeping only the contracts.
pub fn load_components(path: impl AsRef<Path>) -> Result<Vec<ComponentContract>, Error> {
    Ok(load_lowered(path)?
        .into_iter()
        .map(|lowered| lowered.contract)
        .collect())
}

/// Like [`load_components`], but keeps each world's named types alongside
/// its contract (what [`metadata::generate_catalog`] needs).
pub fn load_lowered(path: impl AsRef<Path>) -> Result<Vec<lower::Lowered>, Error> {
    Ok(lower::lower(&load::load_path(path)?)?)
}
