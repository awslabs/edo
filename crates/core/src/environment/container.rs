use async_trait::async_trait;
use dashmap::DashMap;
use edo::context::{Addr, Context, Element, FromElement, Log};
use edo::environment::{EnvResult, Environment, EnvironmentImpl, FarmImpl};
use edo::record;
use edo::source::Source;
use edo::storage::{Id, MediaType, Storage};
use edo::util::{
    Reader, Writer, cmd_collect_out, cmd_noinput, cmd_noredirect, cmd_nulled, from_dash,
};
use snafu::ResultExt;
use snafu::{OptionExt, ensure};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::{File, create_dir_all, remove_file};
use tokio::io::AsyncWriteExt;
use tracing::Instrument;
use uuid::Uuid;
use which::which;

/// Configuration for the container runtime (e.g. which CLI binary to use).
#[derive(serde::Deserialize, Debug, Clone)]
pub struct ContainerConfig {
    runtime: String,
    cli: Option<PathBuf>,
    #[serde(default)]
    network: bool,
}

/// Options for a Container environment
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct ContainerOptions {
    user: String,
    #[serde(flatten)]
    config: Option<ContainerConfig>,
}

/// Container environment farm creates environments that run inside of a container
/// on a container engine like: finch, podman or docker
pub struct ContainerFarm {
    addr: Addr,
    config: ContainerConfig,
    options: ContainerOptions,
    source: Source,
}

#[async_trait]
impl FromElement for ContainerFarm {
    type Error = edo::environment::error::EnvironmentError;

    async fn new(element: &Element, ctx: &Context) -> EnvResult<Self> {
        let options: ContainerOptions = element.get()?;
        let mut config = if let Some(config) = options.config.as_ref() {
            config.clone()
        } else if let Some(config) = ctx.config().get("container") {
            serde_json::from_value(config).context(error::ConfigSnafu {
                addr: element.addr.clone(),
            })?
        } else {
            let (runtime, cli) = if let Ok(finch) = which("finch") {
                ("finch", finch)
            } else if let Ok(podman) = which("podman") {
                ("podman", podman)
            } else if let Ok(docker) = which("docker") {
                ("docker", docker)
            } else {
                return error::NoRuntimeSnafu {}.fail().map_err(|e| e.into());
            };
            info!("found container runtime at: {cli:?}");
            ContainerConfig {
                runtime: runtime.to_string(),
                cli: Some(cli),
                network: false,
            }
        };
        if config.cli.is_none() {
            config.cli = Some(which(&config.runtime).ok().context(error::NoRuntimeSnafu)?);
        }
        let src_element = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .and_then(|map| map.get("default"))
            .and_then(|list| list.first())
            .context(error::NoSourceSnafu)?;
        let source = ctx.add_source(src_element).await?;
        Ok(Self {
            addr: element.addr.clone(),
            config,
            options,
            source,
        })
    }
}

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
        let cli = self.config.cli.as_ref().unwrap();
        if cmd_nulled(
            ".",
            cli,
            ["image", "inspect", name.as_str()],
            &HashMap::new(),
        )
        .context(error::RuntimeSnafu)?
        {
            info!(component = "environment", type = "container", "image already loaded into container engine, if this is incorrect please remove {name} first.");
            return Ok(());
        }
        // The image source stores an oci image as an oci archive in the first layer
        let layer = artifact.layers().first().unwrap();
        let mut reader = storage.safe_read(&layer).await?;

        let path = env::temp_dir().join(Uuid::now_v7().to_string());
        let mut archive = File::create(&path).await.context(error::IoSnafu)?;
        tokio::io::copy(&mut reader, &mut archive)
            .await
            .context(error::IoSnafu)?;
        drop(archive);

        async move {
            // Now we can load the image into the runtime using docker load then tag it accordingly
            record!(log, "load_image", "{:?} load -i {path:?}", cli);
            let output = cmd_collect_out(
                ".",
                log,
                cli,
                ["load", "-i", path.to_str().unwrap()],
                &HashMap::new(),
            )
            .context(error::RuntimeSnafu)?;
            // The return will be the image digest
            let string = String::from_utf8_lossy(output.as_slice());
            let string = string
                .strip_prefix("Loaded image: sha256:")
                .unwrap_or(string.as_ref());
            record!(log, "tag_image", "{:?} tag {} {name}", cli, string.trim());
            cmd_noinput(
                ".",
                log,
                cli,
                ["tag", string.trim(), name.as_str()],
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
        Ok(Environment::new(Container {
            name,
            config: self.config.clone(),
            user: self.options.user.clone(),
            path: path.to_path_buf(),
            running: AtomicBool::new(false),
            tag: image_tag,
            env: DashMap::new(),
        }))
    }
}

/// A running container environment instance managed by a container runtime.
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

    async fn setup(&self, log: &Log, _storage: &Storage) -> EnvResult<()> {
        // make the directory we want exists
        if !self.path.exists() {
            trace!(component = "environment", type = "container", "creating workspace directory at {}", self.path.display());
            record!(log, "create_dir", "{:?}", self.path);
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
            let cli = self.config.cli.as_ref().unwrap();
            let mut args = vec![
                "run".to_string(),
                "-it".to_string(),
                "-d".to_string(),
                "--security-opt".to_string(),
                "label=disable".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
            ];
            if !self.config.network {
                args.push("--network=none".to_string());
            }
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
            record!(log, "launch", "{:?} {}", cli, args.join(" "));
            edo::util::cmd_noinput(".", log, cli, args, &from_dash(&self.env))
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
        let cli = self.config.cli.as_ref().unwrap();
        record!(log, "stop", "{:?} kill {}", cli, self.name);
        edo::util::cmd_noinput(
            ".",
            log,
            cli,
            vec!["kill".into(), self.name.clone()],
            &from_dash(&self.env),
        )
        .context(error::RuntimeSnafu)?;
        record!(log, "clean", "{:?} rm {}", cli, self.name);
        edo::util::cmd_noinput(
            ".",
            log,
            cli,
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

    async fn write_bytes(&self, path: &Path, buffer: &[u8]) -> EnvResult<()> {
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
        file.write_all(buffer)
            .await
            .context(error::WriteFileSnafu)?;
        drop(file);
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
        trace!(component = "environment", type = "container", "writing contents to file at {}", file_path.display());
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
                trace!(component = "environment", type = "container", "unpacking zip into {}", file_path.display());
                super::extract_zip_stream(&file_path, reader).await?;
            }
            _ => {
                trace!(component = "environment", type = "container", "unpacking archive into {}", file_path.display());
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

    async fn read_stream(&self, path: &Path, mut writer: Writer) -> EnvResult<()> {
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

    fn shell(&self, path: &Path) -> EnvResult<()> {
        let work_dir = Path::new("/root").join(path);
        let cli = self.config.cli.as_ref().unwrap();
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
        cmd_noredirect(".", cli, run_args, &from_dash(&self.env)).context(error::RuntimeSnafu)?;
        Ok(())
    }

    async fn execute(&self, log: &Log, id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        let work_dir = Path::new("/root").join(path);
        trace!(component = "environment", type = "container", "running command in {}", work_dir.display());
        async move {
            let cli = self.config.cli.as_ref().unwrap();
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
            run_args.push("-c".into());
            run_args.push(cmd.into());
            record!(log, "exec", "{:?} {}", cli, run_args.join(" "));
            edo::util::cmd_noinput(".", log, cli, run_args, &from_dash(&self.env))
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
}

pub mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    use edo::{
        context::{Addr, error::ContextError},
        environment::error::EnvironmentError,
    };

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to archive directory: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("element {addr} has invalid configuration: {source}"))]
        Config {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(transparent)]
        Context { source: ContextError },
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
            #[snafu(source(from(edo::source::SourceError, Box::new)))]
            source: Box<edo::source::SourceError>,
        },
        #[snafu(display("{source}"))]
        Storage {
            #[snafu(source(from(edo::storage::StorageError, Box::new)))]
            source: Box<edo::storage::StorageError>,
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

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
