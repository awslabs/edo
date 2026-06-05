//! Transform subsystem.
//!
//! A [`Transform`] converts source artifacts into build artifacts by executing
//! steps inside an [`Environment`](crate::environment::Environment). The
//! scheduler invokes transforms in dependency order, feeding each one a
//! prepared environment and collecting the resulting [`Artifact`](crate::storage::Artifact)
//! via [`TransformStatus`].
//!
//! All fallible operations return [`TransformResult`], with failures modelled
//! by [`TransformError`].

use crate::context::{Addr, Handle, Log};
use crate::environment::Environment;
use crate::storage::{Artifact, Id};
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use std::path::PathBuf;

/// Convenience result alias for fallible transform operations.
pub type TransformResult<T> = std::result::Result<T, error::TransformError>;
pub use error::TransformError;

/// A transform converts source artifacts into build artifacts.
///
/// Implementations define how to fetch dependencies, stage files into an
/// environment, and execute the actual build logic. The scheduler drives
/// transforms through their lifecycle: `prepare` → `stage` → `transform`.
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Transform {
    /// Returns the address of the environment farm to use for execution.
    async fn environment(&self) -> TransformResult<Addr>;
    /// Compute the unique artifact [`Id`] that will represent this transform's output.
    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id>;
    /// Returns addresses of all transforms this one depends on.
    async fn depends(&self) -> TransformResult<Vec<Addr>>;
    /// Prepare the transform by fetching all sources and dependent artifacts into storage.
    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()>;
    /// Stage all required files into the given environment before execution.
    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()>;
    /// Execute the transformation, returning success with the produced artifact or a failure.
    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus;
    /// Returns `true` if a user can open a shell when this transform fails.
    fn can_shell(&self) -> bool;
    /// Open an interactive shell in the environment at the transform's working directory.
    fn shell(&self, env: &Environment) -> TransformResult<()>;
}

/// The outcome of a transform execution.
#[allow(clippy::large_enum_variant)]
pub enum TransformStatus {
    /// The transform completed successfully, producing the given artifact.
    Success(Artifact),
    /// The transform failed but may succeed on retry; includes an optional
    /// working directory path and the underlying error.
    Retryable(Option<PathBuf>, error::TransformError),
    /// The transform failed permanently; includes an optional working
    /// directory path and the underlying error.
    Failed(Option<PathBuf>, error::TransformError),
}

/// Errors produced by the transform subsystem.
pub mod error {
    use snafu::Snafu;

    /// Errors that can occur during transform preparation, staging, or execution.
    ///
    /// Most variants transparently wrap errors from lower subsystems
    /// (context, environment, source, storage) or from plugin implementations.
    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum TransformError {
        /// An opaque error surfaced by a transform implementation (plugin or builtin).
        #[snafu(transparent)]
        Implementation {
            source: Box<dyn snafu::Error + Send + Sync>,
        },
        /// A propagated context-layer error (e.g. project/config issues).
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(crate::context::ContextError, Box::new)))]
            source: Box<crate::context::ContextError>,
        },
        /// A propagated environment-layer error.
        #[snafu(transparent)]
        Environment {
            #[snafu(source(from(crate::environment::EnvironmentError, Box::new)))]
            source: Box<crate::environment::EnvironmentError>,
        },
        /// A propagated source-layer error (fetch/stage failure).
        #[snafu(transparent)]
        Source {
            #[snafu(source(from(crate::source::SourceError, Box::new)))]
            source: Box<crate::source::SourceError>,
        },
        /// A propagated storage-layer error.
        #[snafu(transparent)]
        Storage {
            #[snafu(source(from(crate::storage::StorageError, Box::new)))]
            source: Box<crate::storage::StorageError>,
        },
    }
}

/// Convert a fallible expression into a [`TransformStatus::Failed`] on error.
///
/// Use inside [`Transform::transform`] implementations instead of the `?`
/// operator to automatically wrap errors into the expected return type.
#[macro_export]
macro_rules! transform_err {
    ($expr: expr) => {
        match $expr {
            Ok(data) => data,
            Err(e) => {
                debug!(subsystem = "transform", "wrapped error occurred: {e}");
                return TransformStatus::Failed(None, e.into());
            }
        }
    };
}

pub use transform_err;
