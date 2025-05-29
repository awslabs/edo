use async_trait::async_trait;
use dashmap::DashMap;
use edo_core::context::{Addr, Context, FromNode, Log, Node};
use edo_core::environment::{Command, EnvResult, Environment, EnvironmentImpl, FarmImpl};
use edo_core::non_configurable;
use edo_core::storage::{Id, Storage};
use edo_core::util::{Reader, Writer, cmd, cmd_noinput, cmd_noredirect, from_dash};
use snafu::{ResultExt, ensure};
use std::io::Cursor;
use std::path::absolute;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::fs::create_dir_all;
use tracing::Instrument;

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
        trace!(component = "environment", type = "local", "creating new local environment at path: {}", path.display());
        Ok(Environment::from_impl(LocalEnv {
            path: path.to_path_buf(),
            env: DashMap::new(),
        }))
    }
}

#[async_trait]
impl FromNode for LocalFarm {
    type Error = error::Error;

    async fn from_node(_addr: &Addr, _node: &Node, _ctx: &Context) -> Result<Self, Self::Error> {
        Ok(Self::default())
    }
}

non_configurable!(LocalFarm, error::Error);

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
        trace!(component = "environment", type = "local", "setting environment variable {key} to '{value}'");
        self.env.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn get_env(&self, key: &str) -> Option<String> {
        self.env.get(key).map(|x| x.key().clone())
    }

    async fn setup(&self, _log: &Log, _storage: &Storage) -> EnvResult<()> {
        // make sure the directory we want exists
        if !self.path.exists() {
            trace!(component = "environment", type = "local", "creating environment directory at {}", self.path.display());
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

    async fn clean(&self, _log: &Log) -> EnvResult<()> {
        // Delete the directory
        if self.path.exists() {
            trace!(component = "environment", type = "local", "removing environment directory at {}", self.path.display());
            tokio::fs::remove_dir_all(&self.path)
                .await
                .context(error::RemoveDirectorySnafu)?;
        }
        Ok(())
    }

    async fn create_dir(&self, path: &Path) -> EnvResult<()> {
        let path = self.path.join(path);
        trace!(component = "environment", type = "local", "creating directory at {}", path.display());
        create_dir_all(path)
            .await
            .context(error::CreateDirectorySnafu)?;
        Ok(())
    }

    async fn write(&self, path: &Path, mut reader: Reader) -> EnvResult<()> {
        let file_path = self.path.join(path);
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context(error::CreateDirectorySnafu)?;
            }
        }
        trace!(component = "environment", type = "local", "writing contents to file at {}", file_path.display());
        let mut file = File::create(&file_path)
            .await
            .context(error::CreateFileSnafu)?;
        tokio::io::copy(&mut reader, &mut file)
            .await
            .context(error::WriteFileSnafu)?;
        Ok(())
    }

    async fn unpack(&self, path: &Path, reader: Reader) -> EnvResult<()> {
        let file_path = self.path.join(path);
        if !file_path.exists() {
            tokio::fs::create_dir_all(&file_path)
                .await
                .context(error::CreateDirectorySnafu)?;
        }
        trace!(component = "environment", type = "local", "unpacking archive into {}", file_path.display());
        let mut archive = tokio_tar::ArchiveBuilder::new(reader)
            .set_preserve_permissions(false)
            .build();
        archive
            .unpack(&file_path)
            .await
            .context(error::ExtractSnafu)?;
        Ok(())
    }

    async fn read(&self, path: &Path, mut writer: Writer) -> EnvResult<()> {
        let file_path = self.path.join(path);
        ensure!(
            file_path.exists(),
            error::NotFoundSnafu {
                path: path.to_path_buf()
            }
        );
        if file_path.is_file() {
            trace!(component = "environment", type = "local", "reading file at {}", file_path.display());
            let mut file = File::open(&file_path).await.context(error::ReadFileSnafu)?;
            tokio::io::copy(&mut file, &mut writer)
                .await
                .context(error::ReadFileSnafu)?;
        } else {
            trace!(component = "environment", type = "local", "archiving directory at {}", file_path.display());
            let mut archive = tokio_tar::Builder::new(writer);
            archive
                .append_dir_all(".", &file_path)
                .await
                .context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
        }
        Ok(())
    }

    async fn cmd(&self, log: &Log, id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        let work_dir = self.path.join(path);
        trace!(component = "environment", type = "local", "running command in {}", work_dir.display());
        async move {
            cmd_noinput(&work_dir, log, "sh", ["-c", cmd], &from_dash(&self.env))
                .context(error::FailedSnafu)
        }
        .instrument(info_span!(
            target: "local",
            "execute in environment",
            id = id.to_string(),
            log = log.log_name()
        ))
        .await
        .map_err(|e| e.into())
    }

    async fn run(&self, log: &Log, id: &Id, path: &Path, command: &Command) -> EnvResult<bool> {
        let work_dir = self.path.join(path);
        trace!(component = "environment", type = "local", "running command in {}", work_dir.display());
        let result = async move {
            let script = command.to_string();
            let mut cursor = Cursor::new(script.as_bytes());
            cmd(
                &work_dir,
                log,
                "sh",
                Vec::<String>::new(),
                &mut cursor,
                &from_dash(&self.env),
            )
            .context(error::FailedSnafu)
        }
        .instrument(info_span!(
            target: "local",
            "execute in environment",
            id = id.to_string(),
            log = log.log_name()
        ))
        .await?;
        Ok(result)
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

    use edo_core::{environment::error::EnvironmentError, plugin::error::PluginError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to expand path into an absoluyte path: {source}"))]
        Absolute { source: std::io::Error },
        #[snafu(display("failed to archive directory: {source}"))]
        Archive { source: std::io::Error },
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

    impl From<Error> for PluginError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
