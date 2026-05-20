//! Source subsystem.
//!
//! Defines how source code and dependencies are fetched, cached, and staged
//! into build environments. A [`Source`] knows how to retrieve a single
//! artifact (local path, git repo, OCI image, etc.) while a [`Vendor`]
//! exposes a package registry for semver-based dependency resolution via
//! [`Resolver`].
//!
//! All fallible operations return [`SourceResult`], with failures modelled by
//! [`SourceError`].

use crate::context::Log;
use crate::environment::Environment;
use crate::storage::{Artifact, Id, Storage};
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use std::path::Path;

mod error;
mod require;
mod resolver;
mod vendor;
mod version;

/// Convenience result alias for fallible source operations.
pub type SourceResult<T> = std::result::Result<T, error::SourceError>;
pub use error::SourceError;
pub use require::*;
pub use resolver::*;
pub use vendor::*;
pub use version::*;

/// A source of code or artifacts, whether local or remote.
///
/// Implementations handle fetching and staging for a single kind of source
/// (e.g. git clone, local copy, OCI pull). Use [`Source::cache`] in preference
/// to [`Source::fetch`] to benefit from the local artifact cache.
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Source {
    /// The unique id for this source
    async fn get_unique_id(&self) -> SourceResult<Id>;
    /// Fetch the given source to storage
    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact>;
    /// Stage the source into the given environment and path
    async fn stage(
        &self,
        log: &Log,
        storage: &Storage,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()>;
}

impl Source {
    /// Check the cache if this source already exists, and only if it does not
    /// call fetch to get the artifact. Use this in most cases instead of calling
    /// fetch() as fetch will ALWAYS repull the source.
    pub async fn cache(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        // Now we want to check if our caches already have this artifact
        let id = self.get_unique_id().await?;
        // See if our storage can find this source artifact already
        // Note: we use fetch_source because we want to ensure when this is called
        // the artifact is in the local cache.
        if let Some(artifact) = storage.fetch_source(&id).await? {
            return Ok(artifact.clone());
        }
        // Otherwise perform the fetch
        self.fetch(log, storage).await
    }
}
