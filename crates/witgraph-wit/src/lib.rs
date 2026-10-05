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
/// found in it, keeping only the contracts. Fails if any component world
/// fails to lower (every failure is reported); see [`load_each`].
pub fn load_components(path: impl AsRef<Path>) -> Result<Vec<ComponentContract>, Error> {
    Ok(load_lowered(path)?
        .into_iter()
        .map(|lowered| lowered.contract)
        .collect())
}

/// Like [`load_components`], but keeps each world's named types alongside
/// its contract (what [`metadata::generate_catalog`] needs).
///
/// Like [`load_components`], it fails if any component world fails to
/// lower, discarding the others; [`load_each`] keeps them.
pub fn load_lowered(path: impl AsRef<Path>) -> Result<Vec<lower::Lowered>, Error> {
    Ok(lower::lower(&load::load_path(path)?)?)
}

/// Load a `.wit` file or directory and lower each component world in it on
/// its own: one result per world, so a world that fails does not discard
/// the others (an editor can still offer them). Fails only if the source
/// itself does not load.
pub fn load_each(
    path: impl AsRef<Path>,
) -> Result<Vec<Result<lower::Lowered, lower::LowerError>>, load::LoadError> {
    Ok(lower::lower_each(&load::load_path(path)?))
}
