use async_trait::async_trait;
use dashmap::DashMap;
use edo_core::context::{Addr, Context, Definable, FromNode, Log, Node};
use edo_core::environment::{Command, EnvResult, Environment, EnvironmentImpl, FarmImpl};
use edo_core::source::Source;
use edo_core::storage::{Id, Storage};
use edo_core::util::{Reader, Writer, cmd_noinput, cmd_noredirect, cmd_nulled, from_dash};
use snafu::ResultExt;
use snafu::{OptionExt, ensure};
use std::collections::HashMap;
use std::env;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::{File, create_dir_all, remove_file};
use tracing::Instrument;
use uuid::Uuid;
use which::which;

/// Container environment farm creates environments that run inside of a container
/// on a container engine like: finch, podman or docker
pub struct ContainerFarm {
    config: ContainerConfig,
    addr: Addr,
    user: String,
    source: Source,
}

#[derive(Default, Clone)]
pub struct ContainerConfig {
    runtime: Option<String>,
    cli: PathBuf,
}

#[async_trait]
impl FromNode for ContainerConfig {
    type Error = edo_core::environment::error::EnvironmentError;

    async fn from_node(_addr: &Addr, node: &Node, _: &Context) -> EnvResult<Self> {
        let runtime = node.get("runtime").and_then(|x| x.as_string());
        Ok(Self {
            runtime,
            ..Default::default()
        })
    }
}

#[async_trait]
impl FromNode for ContainerFarm {
    type Error = edo_core::environment::error::EnvironmentError;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> EnvResult<Self> {
        let user = node
            .get("user")
            .and_then(|x| x.as_string())
            .unwrap_or("root".into());
        let source_node = node
            .get("source")
            .or(node.get("wants"))
            .context(error::NoSourceSnafu)?;
        let source = source_node
            .as_list()
            .and_then(|x| x.first().cloned())
            .unwrap();
        let source = ctx.add_source(addr, &source).await?;
        Ok(Self {
            addr: addr.clone(),
            config: ContainerConfig::default(),
            user,
            source,
        })
    }
}

#[async_trait]
impl Definable<edo_core::environment::error::EnvironmentError, ContainerConfig> for ContainerFarm {
    fn key() -> &'static str {
        "container"
    }

    fn set_config(&mut self, config: &ContainerConfig) -> EnvResult<()> {
        self.config = config.clone();
        self.config.cli = if let Some(runtime) = self.config.runtime.as_ref() {
            which(runtime).ok().context(error::NoRuntimeSnafu)?
        } else {
            which("podman")
                .or(which("finch"))
                .or(which("docker"))
                .ok()
                .context(error::NoRuntimeSnafu)?
        };
        info!("found container runtime at: {}", self.config.cli.display());
        Ok(())
    }
}

unsafe impl Send for ContainerFarm {}
unsafe impl Sync for ContainerFarm {}

#[async_trait]
impl FarmImpl for ContainerFarm {
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()> {
        // Fetch our source image
        trace!(component = "environment", type = "container", "fetching image for environments");
        let artifact = self
            .source
            .cache(log, storage)
            .await
            .context(error::SourceSnafu)?;

        // Get the image name tag
        let name = format!(
            "edo-{}",
            self.addr
                .to_string()
                .strip_prefix("//")
                .unwrap_or(self.addr.to_string().as_str())
                .replace('/', "-")
        );
        // First we want to check if the image already exists, if so skip the next step
        trace!(component = "environment", type = "container", "check if the image is already loaded into the container runtime");
        if cmd_nulled(
            ".",
            &self.config.cli,
            ["image", "inspect", name.as_str()],
            &HashMap::new(),
        )
        .context(error::RuntimeSnafu)?
        {
            info!(component = "environment", type = "container", "image already loaded into container engine, if this is incorrect please remove {name} first.");
            return Ok(());
        }
        // The image source stores an oci image by its blobs and doesn't really maintain the image config
        // so to load it into the runtime we want to create a filesystem and import that in as our image.
        // We create a manual tempfile
        let path = env::temp_dir().join(Uuid::now_v7().to_string());
        let mut filesystem = File::create(&path).await.context(error::IoSnafu)?;
        for layer in artifact.layers() {
            info!("blob {}", layer.digest().digest());
            let mut reader = storage.safe_read(layer).await?;
            tokio::io::copy(&mut reader, &mut filesystem)
                .await
                .context(error::IoSnafu)?;
        }
        drop(filesystem);

        // Now that we have chained all the layer readers together we can actually send this to an import
        async move {
            cmd_noinput(
                ".",
                log,
                &self.config.cli,
                ["image", "import", path.to_str().unwrap(), name.as_str()],
                &HashMap::new(),
            )
            .context(error::RuntimeSnafu)?;
            info!("image loaded into container runtime");
            remove_file(&path).await.context(error::IoSnafu)?;
            Ok(())
        }
        .instrument(info_span!(
            target: "container",
            "loading image into container runtime",
            id = artifact.config().id().to_string(),
            log = log.log_name()
        ))
        .await
    }

    async fn create(&self, _log: &Log, path: &Path) -> EnvResult<Environment> {
        trace!(component = "environment", type = "container", "creating new container environment with workspace at {}", path.display());
        // Generate a random name
        let mut generator = names::Generator::default();
        let name = generator.next().unwrap();
        let image_tag = format!(
            "edo-{}",
            self.addr
                .to_string()
                .strip_prefix("//")
                .unwrap_or(self.addr.to_string().as_str())
                .replace('/', "-")
        );
        Ok(Environment::from_impl(Container {
            name,
            config: self.config.clone(),
            user: self.user.clone(),
            path: path.to_path_buf(),
            running: AtomicBool::new(false),
            tag: image_tag,
            env: DashMap::new(),
        }))
    }
}

/// Represents a single container environment
pub struct Container {
    config: ContainerConfig,
    name: String,
    user: String,
    path: PathBuf,
    tag: String,
    running: AtomicBool,
    env: DashMap<String, String>,
}

unsafe impl Send for Container {}
unsafe impl Sync for Container {}

#[async_trait]
impl EnvironmentImpl for Container {
    async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
        Ok(if path.starts_with("/") {
            path.to_path_buf()
        } else if self.user == "root" {
            Path::new("/root").join(path)
        } else {
            Path::new(&format!("/home/{}", self.user)).join(path)
        })
    }

    async fn set_env(&self, key: &str, value: &str) -> EnvResult<()> {
        trace!(component = "environment", type = "container", "setting environment variable {key} to '{value}'");
        self.env.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn get_env(&self, key: &str) -> Option<String> {
        self.env.get(key).map(|x| x.value().clone())
    }

    async fn setup(&self, _log: &Log, _storage: &Storage) -> EnvResult<()> {
        // make the directory we want exists
        if !self.path.exists() {
            trace!(component = "environment", type = "container", "creating workspace directory at {}", self.path.display());
            tokio::fs::create_dir_all(&self.path)
                .await
                .context(error::WorkspaceSnafu)?;
        }
        Ok(())
    }

    async fn up(&self, log: &Log) -> EnvResult<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        async move {
            let mut args = vec![
                "run".to_string(),
                "-it".to_string(),
                "-d".to_string(),
                "--network=none".to_string(),
                "--security-opt".to_string(),
                "label=disable".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
            ];
            if self.user == "root" {
                args.push("--mount".to_string());
                args.push(format!(
                    "src={},dst=/root,type=bind",
                    std::path::absolute(self.path.clone()).unwrap().display()
                ));
                args.push("-u".into());
                args.push("0:0".into());
            } else {
                let home_path = format!("/home/{}", self.user);
                args.push("--mount".into());
                args.push(format!(
                    "src={},dst={home_path},type=bind",
                    std::path::absolute(self.path.clone()).unwrap().display()
                ));
            }
            if !self.env.is_empty() {
                args.push("--env".into());
                let env_list = self
                    .env
                    .iter()
                    .map(|x| format!("{}={}", x.key(), x.value()))
                    .collect::<Vec<_>>()
                    .join(",");
                args.push(env_list);
            }
            args.push("--name".into());
            args.push(self.name.clone());
            args.push(self.tag.clone());
            args.push("sh".into());
            edo_core::util::cmd_noinput(".", log, &self.config.cli, args, &from_dash(&self.env))
                .context(error::RuntimeSnafu)?;
            self.running.store(true, Ordering::SeqCst);
            Ok::<(), error::Error>(())
        }
        .instrument(info_span!(target: "container", "spinning up container", log = log.log_name()))
        .await?;
        Ok(())
    }

    async fn down(&self, log: &Log) -> EnvResult<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        edo_core::util::cmd_noinput(
            ".",
            log,
            &self.config.cli,
            vec!["kill".into(), self.name.clone()],
            &from_dash(&self.env),
        )
        .context(error::RuntimeSnafu)?;
        edo_core::util::cmd_noinput(
            ".",
            log,
            &self.config.cli,
            vec!["rm".into(), self.name.clone()],
            &from_dash(&self.env),
        )
        .context(error::RuntimeSnafu)?;
        self.running.store(false, Ordering::SeqCst);
        // No spindown needed for a finch environment
        Ok(())
    }

    async fn clean(&self, _log: &Log) -> EnvResult<()> {
        Ok(())
    }

    async fn create_dir(&self, path: &Path) -> EnvResult<()> {
        let path = self.path.join(path);
        trace!(component = "environment", type = "container", "creating directory at {}", path.display());
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
        trace!(component = "environment", type = "container", "writing contents to file at {}", file_path.display());
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
        trace!(component = "environment", type = "container", "unpacking archive into {}", file_path.display());
        let mut archive = tokio_tar::Archive::new(reader);
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
            trace!(component = "environment", type = "container", "reading file at {}", file_path.display());
            let mut file = File::open(&file_path).await.context(error::ReadFileSnafu)?;
            tokio::io::copy(&mut file, &mut writer)
                .await
                .context(error::ReadFileSnafu)?;
        } else {
            trace!(component = "environment", type = "container", "archiving directory at {}", file_path.display());
            let mut archive = tokio_tar::Builder::new(writer);
            archive
                .append_dir_all(".", &file_path)
                .await
                .context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
        }
        Ok(())
    }

    fn shell(&self, path: &Path) -> EnvResult<()> {
        let work_dir = Path::new("/root").join(path);
        let mut args = vec![
            "exec".to_string(),
            "-it".to_string(),
            "--workdir".to_string(),
            format!("{}", work_dir.display()),
        ];
        if self.user == "root" {
            args.push("-u".into());
            args.push("0:0".into());
        }
        if !self.env.is_empty() {
            args.push("--env".into());
            let env_list = self
                .env
                .iter()
                .map(|x| format!("{}={}", x.key(), x.value()))
                .collect::<Vec<_>>()
                .join(",");
            args.push(env_list);
        }
        args.push(self.name.clone());
        let mut run_args = args.clone();
        run_args.push("sh".into());
        cmd_noredirect(".", &self.config.cli, run_args, &from_dash(&self.env))
            .context(error::RuntimeSnafu)?;
        Ok(())
    }

    async fn cmd(&self, log: &Log, id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        let work_dir = Path::new("/root").join(path);
        trace!(component = "environment", type = "container", "running command in {}", work_dir.display());
        async move {
            let mut args = vec![
                "exec".to_string(),
                //"-i".to_string(),
                "--workdir".to_string(),
                format!("{}", work_dir.display()),
            ];
            if self.user == "root" {
                args.push("-u".into());
                args.push("0:0".into());
            }
            if !self.env.is_empty() {
                args.push("--env".into());
                let env_list = self
                    .env
                    .iter()
                    .map(|x| format!("{}={}", x.key(), x.value()))
                    .collect::<Vec<_>>()
                    .join(",");
                args.push(env_list);
            }
            args.push(self.name.clone());
            let mut run_args = args.clone();
            run_args.push(
                cmd.to_string(), /*format!("sh -c '{}'", cmd.replace("'", "\'"))*/
            );
            edo_core::util::cmd_noinput(".", log, &self.config.cli, run_args, &from_dash(&self.env))
                .context(error::RuntimeSnafu)
        }
        .instrument(info_span!(
            target: "container",
            "executing in environment",
            id = id.to_string(),
            log = log.log_name()
        ))
        .await
        .map_err(|e| e.into())
    }

    async fn run(&self, log: &Log, id: &Id, path: &Path, command: &Command) -> EnvResult<bool> {
        let work_dir = Path::new("/root").join(path);
        trace!(component = "environment", type = "container", "running command in {}", work_dir.display());
        async move {
            let mut args = vec![
                "exec".to_string(),
                "-i".to_string(),
                "--workdir".to_string(),
                format!("{}", work_dir.display()),
            ];
            if self.user == "root" {
                args.push("-u".into());
                args.push("0:0".into());
            }
            if !self.env.is_empty() {
                args.push("--env".into());
                let env_list = self
                    .env
                    .iter()
                    .map(|x| format!("{}={}", x.key(), x.value()))
                    .collect::<Vec<_>>()
                    .join(",");
                args.push(env_list);
            }
            args.push(self.name.clone());
            let mut run_args = args.clone();
            run_args.push("sh".into());
            let script = command.to_string();
            let mut cursor = Cursor::new(script.as_bytes());
            Ok(edo_core::util::cmd(
                ".",
                log,
                &self.config.cli,
                run_args,
                &mut cursor,
                &from_dash(&self.env),
            )
            .context(error::RuntimeSnafu)?)
        }
        .instrument(info_span!(
            target: "container",
            "executing in environment",
            id = id.to_string(),
            log = log.log_name()
        ))
        .await
    }
}

pub mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    use edo_core::{environment::error::EnvironmentError, plugin::error::PluginError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to archive directory: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("failed to create directory: {source}"))]
        CreateDirectory { source: std::io::Error },
        #[snafu(display("failed to create file: {source}"))]
        CreateFile { source: std::io::Error },
        #[snafu(display("failed to extract archive: {source}"))]
        Extract { source: std::io::Error },
        #[snafu(display("io error occured setting up container environment: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to load oci image into container runtime: {source}"))]
        Load { source: std::io::Error },
        #[snafu(display(
            "no supported container runtime was found, make sure one of podman, finch or docker is available"
        ))]
        NoRuntime,
        #[snafu(display("container environments must have a source"))]
        NoSource,
        #[snafu(display("file does not exist: {}", path.display()))]
        NotFound { path: PathBuf },
        #[snafu(display("failed to read file: {source}"))]
        ReadFile { source: std::io::Error },
        #[snafu(display("failed to execute runtime: {source}"))]
        Runtime { source: std::io::Error },
        #[snafu(display("{source}"))]
        Source {
            #[snafu(source(from(edo_core::source::SourceError, Box::new)))]
            source: Box<edo_core::source::SourceError>,
        },
        #[snafu(display("{source}"))]
        Storage {
            #[snafu(source(from(edo_core::storage::StorageError, Box::new)))]
            source: Box<edo_core::storage::StorageError>,
        },
        #[snafu(display("artifact does not have an image tag in its metadata"))]
        TagMissing,
        #[snafu(display("failed to create workspace directory: {source}"))]
        Workspace { source: std::io::Error },
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
