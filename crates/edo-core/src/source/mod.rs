use crate::context::Log;
use crate::def_trait;
use crate::environment::Environment;
use crate::storage::{Artifact, Id, Storage};
use std::path::Path;

mod error;
mod require;
mod resolver;
mod vendor;
mod version;

pub type SourceResult<T> = std::result::Result<T, error::SourceError>;
pub use error::SourceError;
pub use require::*;
pub use resolver::*;
pub use vendor::*;
pub use version::*;

def_trait! {
    "This trait represents the interface all source implementations should follow" =>
    "A handle to a given implementation of a source" =>
    Source: SourceImpl {
        "The unique id for this source" =>
        get_unique_id() -> SourceResult<Id>;
        "Fetch the given source to storage" =>
        fetch(log: &Log, storage: &Storage) -> SourceResult<Artifact>;
        "Stage the source into the given environment and path" =>
        stage(log: &Log, storage: &Storage, env: &Environment, path: &Path) -> SourceResult<()>
    }
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
