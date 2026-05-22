//! Environment subsystem.
//!
//! Defines where transforms execute. An [`Environment`] provides sandboxing,
//! filesystem operations, and command execution; a [`Farm`] creates fresh
//! environments on demand for the scheduler. [`Command`] captures a deferred
//! script (interpreter + handlebars-templated commands + variables) that is
//! later dispatched to an [`Environment`] via [`Environment::run`].
//!
//! All fallible operations return [`EnvResult`], with failures modelled by
//! [`EnvironmentError`] in [`error`].

use super::storage::Id;
use super::storage::Storage;
use crate::context::Handle;
use crate::context::Log;
use crate::storage::ArtifactStageOptions;
use crate::util::{Reader, Writer};
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use std::path::{Path, PathBuf};

pub mod error;
mod farm;
mod vfs;

pub use error::EnvironmentError;
pub use farm::*;
pub use vfs::*;

/// Convenience result alias for fallible environment operations.
pub type EnvResult<T> = std::result::Result<T, error::EnvironmentError>;

/// An Environment represents where a transform is executed and generally outside of local environments provide some level of sandboxing
/// and isolation.
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Environment {
    /// Expand the provided path to a canonicalized absolute path inside of an environment
    async fn expand(&self, path: &Path) -> EnvResult<PathBuf>;
    /// Create a directory inside of the environment
    async fn create_dir(&self, path: &Path) -> EnvResult<()>;
    /// Set an environment variable
    async fn set_env(&self, key: &str, value: &str) -> EnvResult<()>;
    /// Get an environment variable
    async fn get_env(&self, key: &str) -> Option<String>;
    /// Setup the environment for execution
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()>;
    /// Spin the environment up
    async fn up(&self, log: &Log) -> EnvResult<()>;
    /// Spin the environment down
    async fn down(&self, log: &Log) -> EnvResult<()>;
    /// Cleanup the environment
    async fn clean(&self, log: &Log) -> EnvResult<()>;

    // -- IO Operations --

    async fn write_bytes(&self, path: &Path, buffer: &[u8]) -> EnvResult<()>;
    async fn write_stream(&self, path: &Path, reader: Reader) -> EnvResult<()>;
    async fn unpack_stream(&self, path: &Path, reader: Reader) -> EnvResult<()>;
    async fn read_bytes(&self, path: &Path) -> EnvResult<Vec<u8>>;
    async fn read_stream(&self, path: &Path, writer: Writer) -> EnvResult<()>;
    async fn execute(&self, log: &Log, id: &Id, path: &Path, command: &str) -> EnvResult<bool>;
    /// Open a shell in the environment
    fn shell(&self, path: &Path) -> EnvResult<()>;
}

impl Environment {
    /// Helper that stages an artifact from storage into an environment
    /// using the media_type to determine how
    pub async fn stage(&self, ctx: &Handle, options: ArtifactStageOptions) -> EnvResult<()> {
        let artifact = ctx.storage().safe_open(options.id()).await?;
        for layer in artifact.layers() {
            let mut reader = ctx.storage().safe_read(layer).await?;
            if layer.media_type().is_compressed() && options.decompress() {
                reader = Reader::with_decompression(reader, &layer.media_type().compression());
            }
            if layer.media_type().is_archive() && options.extract() {
                let path = if let Some(hint) = layer.path_hint() {
                    options.path().join(hint)
                } else {
                    options.path().to_path_buf()
                };
                self.unpack_stream(&path, reader).await?;
            } else {
                // We assume path is a directory if we are writing a file we need to pick a filename
                // we do this by seeing if a filename has been set
                let filename = layer
                    .path_hint()
                    .clone()
                    .unwrap_or(PathBuf::from(layer.digest().digest()));
                let filepath = options.path().join(filename);
                self.write_stream(&filepath, reader).await?;
            }
        }
        Ok(())
    }
}
