use async_trait::async_trait;
use dashmap::DashMap;
use edo::context::{Context, Element, FromElement, Log};
use edo::environment::{EnvResult, Environment, EnvironmentImpl, FarmImpl};
use edo::record;
use edo::storage::{Id, MediaType, Storage};
use edo::util::{Reader, Writer, cmd_noinput, cmd_noredirect, from_dash};
use snafu::{ResultExt, ensure};
use std::path::absolute;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::fs::create_dir_all;
use tracing::Instrument;

/// A farm that creates local (host-native) build environments.
#[derive(Default)]
pub struct LocalFarm {}

unsafe impl Send for LocalFarm {}
unsafe impl Sync for LocalFarm {}

#[async_trait]
impl FarmImpl for LocalFarm {
    async fn setup(&self, _log: &Log, _storage: &Storage) -> EnvResult<()> {
        Ok(())
    }

    async fn create(&self, _log: &Log, path: &Path) -> EnvResult<Environment> {
        trace!(
            subsystem = "environment",
            component = "local",
            path = %path.display(),
            "creating new local environment"
        );
        Ok(Environment::new(LocalEnv {
            path: path.to_path_buf(),
            env: DashMap::new(),
        }))
    }
}

#[async_trait]
impl FromElement for LocalFarm {
    type Error = error::Error;

    async fn new(_element: &Element, _ctx: &Context) -> Result<Self, Self::Error> {
        Ok(Self::default())
    }
}

/// A local build environment rooted at a filesystem path.
pub struct LocalEnv {
    path: PathBuf,
    env: DashMap<String, String>,
}

unsafe impl Send for LocalEnv {}
unsafe impl Sync for LocalEnv {}

#[async_trait]
impl EnvironmentImpl for LocalEnv {
    async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
        ensure!(
            !path.starts_with("/") || path.starts_with(&self.path),
            error::MutateSnafu {
                path: path.to_path_buf()
            }
        );
        if path.starts_with(&self.path) {
            return Ok(path.to_path_buf());
        }
        absolute(self.path.join(path))
            .context(error::AbsoluteSnafu)
            .map_err(|e| e.into())
    }

    async fn set_env(&self, key: &str, value: &str) -> EnvResult<()> {
        trace!(
            subsystem = "environment",
            component = "local",
            op = "set-env",
            key = %key,
            value = %value,
            "setting environment variable"
        );
        self.env.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn get_env(&self, key: &str) -> Option<String> {
        self.env.get(key).map(|x| x.value().clone())
    }

    async fn setup(&self, log: &Log, _storage: &Storage) -> EnvResult<()> {
        // make sure the directory we want exists
        if !self.path.exists() {
            trace!(
                subsystem = "environment",
                component = "local",
                op = "create-dir",
                path = %self.path.display(),
                "creating environment directory"
            );
            record!(
                log,
                "create_dir",
                "creating environment directory at {:?}",
                self.path
            );
            tokio::fs::create_dir_all(&self.path)
                .await
                .context(error::CreateDirectorySnafu)?;
        }

        Ok(())
    }

    async fn up(&self, _log: &Log) -> EnvResult<()> {
        // No spinup needed for a local environment
        Ok(())
    }

    async fn down(&self, _log: &Log) -> EnvResult<()> {
        // No spindown needed for a local environment
        Ok(())
    }

    async fn clean(&self, log: &Log) -> EnvResult<()> {
        // Delete the directory
        if self.path.exists() {
            trace!(
                subsystem = "environment",
                component = "local",
                op = "remove-dir",
                path = %self.path.display(),
                "removing environment directory"
            );
            record!(
                log,
                "remove_dir",
                "removing environment directory at {:?}",
                self.path
            );
            tokio::fs::remove_dir_all(&self.path)
                .await
                .context(error::RemoveDirectorySnafu)?;
        }
        Ok(())
    }

    async fn create_dir(&self, path: &Path) -> EnvResult<()> {
        let path = self.path.join(path);
        trace!(
            subsystem = "environment",
            component = "local",
            op = "create-dir",
            path = %path.display(),
            "creating directory"
        );
        create_dir_all(path)
            .await
            .context(error::CreateDirectorySnafu)?;
        Ok(())
    }

    async fn write_bytes(&self, path: &Path, buffer: &[u8]) -> EnvResult<()> {
        let file_path = self.path.join(path);
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context(error::CreateDirectorySnafu)?;
            }
        }
        trace!(
            subsystem = "environment",
            component = "local",
            op = "write-file",
            path = %file_path.display(),
            "writing contents to file"
        );
        tokio::fs::write(&file_path, buffer)
            .await
            .context(error::CreateFileSnafu)?;
        Ok(())
    }

    async fn write_stream(&self, path: &Path, mut reader: Reader) -> EnvResult<()> {
        let file_path = self.path.join(path);
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context(error::CreateDirectorySnafu)?;
            }
        }
        trace!(
            subsystem = "environment",
            component = "local",
            op = "write-file",
            path = %file_path.display(),
            "writing contents to file"
        );
        let mut file = File::create(&file_path)
            .await
            .context(error::CreateFileSnafu)?;
        tokio::io::copy(&mut reader, &mut file)
            .await
            .context(error::WriteFileSnafu)?;
        Ok(())
    }

    async fn unpack_stream(
        &self,
        path: &Path,
        media_type: &MediaType,
        reader: Reader,
    ) -> EnvResult<()> {
        let file_path = self.path.join(path);
        if !file_path.exists() {
            tokio::fs::create_dir_all(&file_path)
                .await
                .context(error::CreateDirectorySnafu)?;
        }
        match media_type {
            MediaType::Zip(..) => {
                trace!(
                    subsystem = "environment",
                    component = "local",
                    op = "unpack-zip",
                    path = %file_path.display(),
                    "unpacking zip"
                );
                super::extract_zip_stream(&file_path, reader).await?;
            }
            _ => {
                trace!(
                    subsystem = "environment",
                    component = "local",
                    op = "unpack",
                    path = %file_path.display(),
                    "unpacking archive"
                );
                let mut archive = tokio_tar::ArchiveBuilder::new(reader)
                    .set_preserve_permissions(true)
                    .build();
                archive
                    .unpack(&file_path)
                    .await
                    .context(error::ExtractSnafu)?;
            }
        }
        Ok(())
    }

    async fn read_bytes(&self, path: &Path) -> EnvResult<Vec<u8>> {
        let file_path = self.path.join(path);
        ensure!(
            file_path.exists(),
            error::NotFoundSnafu {
                path: path.to_path_buf()
            }
        );
        Ok(tokio::fs::read(&file_path)
            .await
            .context(error::ReadFileSnafu)?)
    }

    async fn read_stream(&self, path: &Path, mut writer: Writer) -> EnvResult<()> {
        let file_path = self.path.join(path);
        ensure!(
            file_path.exists(),
            error::NotFoundSnafu {
                path: path.to_path_buf()
            }
        );
        if file_path.is_file() {
            trace!(
                subsystem = "environment",
                component = "local",
                op = "read-file",
                path = %file_path.display(),
                "reading file"
            );
            let mut file = File::open(&file_path).await.context(error::ReadFileSnafu)?;
            tokio::io::copy(&mut file, &mut writer)
                .await
                .context(error::ReadFileSnafu)?;
        } else {
            trace!(
                subsystem = "environment",
                component = "local",
                op = "archive",
                path = %file_path.display(),
                "archiving directory"
            );
            let mut archive = tokio_tar::Builder::new(writer);
            archive
                .append_dir_all(".", &file_path)
                .await
                .context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
        }
        Ok(())
    }

    async fn execute(&self, log: &Log, id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        let work_dir = self.path.join(path);
        trace!(
            subsystem = "environment",
            component = "local",
            op = "exec",
            path = %work_dir.display(),
            "running command"
        );
        async move {
            record!(log, "exec", "sh -c {cmd}");
            cmd_noinput(&work_dir, log, "sh", ["-c", cmd], &from_dash(&self.env))
                .context(error::FailedSnafu)
        }
        .instrument(info_span!(
            "local-exec",
            subsystem = "environment",
            component = "local",
            id = %id
        ))
        .await
        .map_err(|e| e.into())
    }

    fn shell(&self, path: &Path) -> EnvResult<()> {
        let work_dir = self.path.join(path);
        cmd_noredirect(&work_dir, "sh", Vec::<String>::new(), &from_dash(&self.env))
            .context(error::FailedSnafu)?;
        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    use edo::{context::error::ContextError, environment::error::EnvironmentError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to expand path into an absoluyte path: {source}"))]
        Absolute { source: std::io::Error },
        #[snafu(display("failed to archive directory: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(transparent)]
        Context { source: ContextError },
        #[snafu(display("failed to create a file: {source}"))]
        CreateFile { source: std::io::Error },
        #[snafu(display("failed to create a directory: {source}"))]
        CreateDirectory { source: std::io::Error },
        #[snafu(display("failed to extract archive: {source}"))]
        Extract { source: std::io::Error },
        #[snafu(display("command failed to execute: {source}"))]
        Failed { source: std::io::Error },
        #[snafu(display("cannot mutate things in a root path: {}", path.display()))]
        Mutate { path: PathBuf },
        #[snafu(display("file at path {} does not exist", path.display()))]
        NotFound { path: PathBuf },
        #[snafu(display("no path provided to create local environments inside"))]
        PathRequired,
        #[snafu(display("failed to read file: {source}"))]
        ReadFile { source: std::io::Error },
        #[snafu(display("failed to remove a directory: {source}"))]
        RemoveDirectory { source: std::io::Error },
        #[snafu(display("failed to write to file: {source}"))]
        WriteFile { source: std::io::Error },
    }

    impl From<Error> for EnvironmentError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
