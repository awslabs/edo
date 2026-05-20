use super::EnvResult;
use super::Environment;
use crate::context::Log;
use crate::storage::Storage;
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use std::path::Path;

/// An Environment farm determines how to create new build environments for a transform
/// to run in. Implementations should implement FarmImpl
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Farm {
    /// Setup can be used for any one time initializations required for a farm
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()>;
    /// Create a new environment using this farm
    async fn create(&self, log: &Log, path: &Path) -> EnvResult<Environment>;
}
