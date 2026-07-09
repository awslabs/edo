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
use crate::storage::{ArtifactStageOptions, MediaType};
use crate::util::{Reader, Writer};
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use std::path::{Path, PathBuf};

/// Archive suffixes recognized by [`resolved_stage_subpath`], ordered
/// longest-first so compound suffixes like `.tar.gz` win over their base
/// `.tar` form.
const ARCHIVE_SUFFIXES: &[&str] = &[
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz", ".tar", ".tgz", ".tbz2", ".txz",
    ".tzst", ".zip",
];

/// Returns the path the staging hint should resolve to at extract time.
///
/// For archive layers being extracted, strips a single trailing archive
/// suffix (`.tar`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst`,
/// `.tar.lz`, `.tgz`, `.tbz2`, `.txz`, `.tzst`, `.zip`) from the hint's
/// file-name component so the contents land in a directory named after
/// the logical artifact rather than after the archive file. Any parent
/// directories encoded in the hint are preserved.
///
/// Non-archive media types and degenerate inputs (e.g. a file name that
/// is itself just an archive suffix) are passed through unchanged.
///
/// This function does not mutate persisted artifact state — it is only
/// consulted at staging time, so the on-disk artifact catalog and any
/// existing caches are unaffected.
pub fn resolved_stage_subpath(hint: &Path, media_type: &MediaType) -> PathBuf {
    if !media_type.is_archive() {
        return hint.to_path_buf();
    }
    let Some(file_name) = hint.file_name().and_then(|n| n.to_str()) else {
        return hint.to_path_buf();
    };
    for suffix in ARCHIVE_SUFFIXES {
        if let Some(stripped) = file_name.strip_suffix(suffix) {
            if stripped.is_empty() {
                warn!(
                    subsystem = "environment",
                    component = "stage",
                    hint = %hint.display(),
                    "archive path hint is just a suffix; staging to original hint"
                );
                return hint.to_path_buf();
            }
            return hint.with_file_name(stripped);
        }
    }
    hint.to_path_buf()
}

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
    async fn unpack_stream(
        &self,
        path: &Path,
        media_type: &MediaType,
        reader: Reader,
    ) -> EnvResult<()>;
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
            let hint = artifact.config().path_hint_for(layer.digest()).cloned();
            if layer.media_type().is_archive() && options.extract() {
                let path = if !options.ignore_artifact_path()
                    && let Some(hint) = hint
                {
                    let resolved = resolved_stage_subpath(&hint, layer.media_type());
                    options.path().join(resolved)
                } else {
                    options.path().to_path_buf()
                };
                self.unpack_stream(&path, layer.media_type(), reader)
                    .await?;
            } else {
                // We assume path is a directory if we are writing a file we need to pick a filename
                // we do this by seeing if a filename has been set
                let mut filepath = options.path().to_path_buf();
                if !options.ignore_artifact_path() {
                    let filename = hint.unwrap_or(PathBuf::from(layer.digest().digest()));
                    filepath = filepath.join(filename);
                }

                self.write_stream(&filepath, reader).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Compression;

    fn check(hint: &str, media_type: MediaType, expected: &str) {
        let actual = resolved_stage_subpath(Path::new(hint), &media_type);
        assert_eq!(
            actual,
            PathBuf::from(expected),
            "resolved_stage_subpath({hint:?}, {media_type:?}) = {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn strips_tar_gz_suffix() {
        check("foo.tar.gz", MediaType::Tar(Compression::Gzip), "foo");
    }

    #[test]
    fn strips_tgz_suffix() {
        check("foo.tgz", MediaType::Tar(Compression::Gzip), "foo");
    }

    #[test]
    fn strips_zip_suffix() {
        check("foo.zip", MediaType::Zip(Compression::None), "foo");
    }

    #[test]
    fn passes_through_non_archive() {
        check("foo.txt", MediaType::File(Compression::None), "foo.txt");
    }

    #[test]
    fn preserves_parent_directories() {
        check(
            "vendor/foo.tar.gz",
            MediaType::Tar(Compression::Gzip),
            "vendor/foo",
        );
    }

    #[test]
    fn strips_only_one_suffix() {
        check(
            "foo.tar.tar.gz",
            MediaType::Tar(Compression::Gzip),
            "foo.tar",
        );
    }

    #[test]
    fn passes_through_degenerate_suffix_only_filename() {
        check(".tar.gz", MediaType::Tar(Compression::Gzip), ".tar.gz");
    }

    #[test]
    fn strips_tar_bz2_suffix() {
        check("foo.tar.bz2", MediaType::Tar(Compression::Bzip2), "foo");
    }

    #[test]
    fn strips_tar_xz_suffix() {
        check("foo.tar.xz", MediaType::Tar(Compression::Xz), "foo");
    }

    #[test]
    fn strips_tar_zst_suffix() {
        check("foo.tar.zst", MediaType::Tar(Compression::Zstd), "foo");
    }

    #[test]
    fn strips_plain_tar_suffix() {
        check("foo.tar", MediaType::Tar(Compression::None), "foo");
    }

    #[test]
    fn passes_through_unknown_suffix_on_archive() {
        // .rar isn't in the table — pass through.
        check("foo.rar", MediaType::Tar(Compression::None), "foo.rar");
    }
}
