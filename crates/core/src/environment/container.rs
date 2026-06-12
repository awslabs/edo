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
///
/// All fields are optional; an unset `runtime` triggers autodetection
/// (`finch` -> `podman` -> `docker`), and an unset `cli` is resolved via
/// `which(runtime)`.
#[derive(serde::Deserialize, Debug, Clone, Default)]
pub struct ContainerConfig {
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    cli: Option<PathBuf>,
    #[serde(default)]
    network: bool,
    /// Maximum number of processes/threads the container may spawn.
    ///
    /// Maps to `--pids-limit`. `-1` (the default) means unlimited; rootless
    /// runtimes otherwise inherit a low cap (often 2048) which fork-heavy
    /// builds (Go, Kubernetes) trivially exhaust with EAGAIN from `fork(2)`.
    /// Set to `None` to omit the flag and use the runtime's default.
    #[serde(default = "default_pids_limit")]
    pids_limit: i64,
    /// Per-container ulimits, each formatted as `name=soft[:hard]` and passed
    /// through verbatim as repeated `--ulimit` flags (e.g. `"nproc=65535"`,
    /// `"nofile=1048576"`).
    ///
    /// Defaults provide unlimited `nproc` and a `nofile` value capped at the
    /// host user's hard limit so fork-heavy builds (Go, Kubernetes, parallel
    /// rpmbuild) don't hit `EAGAIN` from rootless `RLIMIT_NPROC`/`RLIMIT_NOFILE`
    /// inherited from the host user. Rootless container runtimes (crun/runc)
    /// invoke `setrlimit(RLIMIT_NOFILE, ...)` inside the user namespace, and
    /// the kernel still enforces the parent's hard limit — exceeding it
    /// triggers `setrlimit RLIMIT_NOFILE: Operation not permitted` at OCI
    /// container start. Any user-supplied entry for the same ulimit name
    /// overrides the default.
    #[serde(default = "default_ulimits")]
    ulimits: Vec<String>,
}

fn default_pids_limit() -> i64 {
    -1
}

/// Read the current process' hard `RLIMIT_NOFILE` from `/proc/self/limits`.
/// Returns `None` if the file is unreadable or the value can't be parsed
/// (e.g. non-Linux, or the entry shows `unlimited`). The caller falls back
/// to omitting the ulimit so the runtime inherits the host default.
fn host_nofile_hard_limit() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/limits").ok()?;
    for line in contents.lines() {
        // Format: "Max open files   <soft>   <hard>   files"
        let Some(rest) = line.strip_prefix("Max open files") else {
            continue;
        };
        let mut cols = rest.split_whitespace();
        let _soft = cols.next()?;
        let hard = cols.next()?;
        return hard.parse::<u64>().ok();
    }
    None
}

fn default_ulimits() -> Vec<String> {
    let mut out = vec!["nproc=-1:-1".to_string()];
    // Cap our preferred 1M nofile target at whatever the host user actually
    // permits; rootless crun/runc cannot raise nofile above the inherited
    // hard limit. If we can't probe the host (non-Linux, /proc not mounted),
    // omit the flag and inherit the runtime's default.
    if let Some(hard) = host_nofile_hard_limit() {
        let target = hard.min(1_048_576);
        out.push(format!("nofile={target}:{target}"));
    }
    out
}

/// Merge user-supplied ulimit entries with the defaults so the user's
/// settings win for any ulimit name they specify, but defaults are still
/// applied for names they didn't set.
fn merge_ulimits(user: &[String]) -> Vec<String> {
    fn name_of(entry: &str) -> &str {
        entry.split('=').next().unwrap_or(entry)
    }
    let user_names: std::collections::HashSet<&str> = user.iter().map(|e| name_of(e)).collect();
    let mut out: Vec<String> = user.to_vec();
    for default in default_ulimits() {
        if !user_names.contains(name_of(&default)) {
            out.push(default);
        }
    }
    out
}

/// Probe `PATH` for a supported container runtime, in priority order.
fn detect_runtime() -> Option<(&'static str, PathBuf)> {
    if let Ok(cli) = which("finch") {
        Some(("finch", cli))
    } else if let Ok(cli) = which("podman") {
        Some(("podman", cli))
    } else if let Ok(cli) = which("docker") {
        Some(("docker", cli))
    } else {
        None
    }
}

/// Options for a Container environment
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct ContainerOptions {
    user: String,
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
        // Precedence: per-environment `config` block, then the project-level
        // `[container]` table, then full defaults. Any unset field in the
        // chosen source falls back to the same autodetect/`which` logic.
        let mut config = if let Some(config) = options.config.as_ref() {
            config.clone()
        } else if let Some(config) = ctx.config().get("container") {
            serde_json::from_value(config).context(error::ConfigSnafu {
                addr: element.addr.clone(),
            })?
        } else {
            ContainerConfig::default()
        };
        if config.runtime.is_none() {
            let (runtime, cli) = detect_runtime().context(error::NoRuntimeSnafu)?;
            info!(
                subsystem = "environment",
                component = "container",
                op = "runtime-detect",
                cli = ?cli,
                runtime = %runtime,
                "found container runtime"
            );
            config.runtime = Some(runtime.to_string());
            // Only fill `cli` from autodetect if the user didn't override it,
            // so an explicit `cli = "..."` keeps working with autodetected
            // runtime kind.
            if config.cli.is_none() {
                config.cli = Some(cli);
            }
        }
        if config.cli.is_none() {
            // SAFETY: runtime is Some at this point.
            let runtime = config.runtime.as_deref().unwrap();
            config.cli = Some(which(runtime).ok().context(error::NoRuntimeSnafu)?);
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
        trace!(
            subsystem = "environment",
            component = "container",
            "fetching image for environments"
        );
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
        trace!(
            subsystem = "environment",
            component = "container",
            "check if the image is already loaded into the container runtime"
        );
        let cli = self.config.cli.as_ref().unwrap();
        if cmd_nulled(
            ".",
            cli,
            ["image", "inspect", name.as_str()],
            &HashMap::new(),
        )
        .context(error::RuntimeSnafu)?
        {
            debug!(
                subsystem = "environment",
                component = "container",
                op = "image-load",
                name = %name,
                "image already loaded into container engine, if this is incorrect please remove {name} first."
            );
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

        let artifact_id = artifact.config().id().to_string();
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
            info!(
                subsystem = "environment",
                component = "container",
                op = "image-load",
                id = %artifact_id,
                "image loaded into container runtime"
            );
            remove_file(&path).await.context(error::IoSnafu)?;
            Ok(())
        }
        .instrument(info_span!(
            "container-load-image",
            subsystem = "environment",
            component = "container",
            id = %artifact.config().id()
        ))
        .await
    }

    async fn create(&self, _log: &Log, path: &Path) -> EnvResult<Environment> {
        trace!(
            subsystem = "environment",
            component = "container",
            path = %path.display(),
            "creating new container environment"
        );
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

impl Container {
    fn local_path(&self, path: &Path) -> PathBuf {
        if self.user == "root"
            && let Ok(stripped) = path.strip_prefix("/root")
        {
            stripped.to_path_buf()
        } else if let Ok(stripped) = path.strip_prefix(format!("/home/{}", self.user)) {
            stripped.to_path_buf()
        } else {
            path.to_path_buf()
        }
    }
}

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
        trace!(
            subsystem = "environment",
            component = "container",
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
        // make the directory we want exists
        if !self.path.exists() {
            trace!(
                subsystem = "environment",
                component = "container",
                op = "create-dir",
                path = %self.path.display(),
                "creating workspace directory"
            );
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
            // Heavy builds (Kubernetes' Go toolchain, parallel rpmbuild, etc.)
            // can blow past the rootless default `pids.max` and fail with
            // `fork/exec ...: resource temporarily unavailable`. Default to
            // unlimited; honour an explicit override if the user set one.
            args.push("--pids-limit".to_string());
            args.push(self.config.pids_limit.to_string());

            // Merge user-configured ulimits with defaults so unset names
            // (typically `nproc`/`nofile`) still get sensible caps.
            for ulimit in merge_ulimits(&self.config.ulimits) {
                args.push("--ulimit".to_string());
                args.push(ulimit);
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
        .instrument(info_span!(
            "container-up",
            subsystem = "environment",
            component = "container"
        ))
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
        let path = self.local_path(&path);
        let path = self.path.join(path);
        trace!(
            subsystem = "environment",
            component = "container",
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
        let path = self.local_path(path);
        let file_path = self.path.join(&path);
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context(error::CreateDirectorySnafu)?;
            }
        }
        trace!(
            subsystem = "environment",
            component = "container",
            op = "write-file",
            path = %file_path.display(),
            "writing contents to file"
        );
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
        let path = self.local_path(path);
        let file_path = self.path.join(&path);
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .context(error::CreateDirectorySnafu)?;
            }
        }
        trace!(
            subsystem = "environment",
            component = "container",
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
        let path = self.local_path(path);
        let file_path = self.path.join(&path);
        if !file_path.exists() {
            tokio::fs::create_dir_all(&file_path)
                .await
                .context(error::CreateDirectorySnafu)?;
        }
        match media_type {
            MediaType::Zip(..) => {
                trace!(
                    subsystem = "environment",
                    component = "container",
                    op = "unpack-zip",
                    path = %file_path.display(),
                    "unpacking zip"
                );
                super::extract_zip_stream(&file_path, reader).await?;
            }
            _ => {
                trace!(
                    subsystem = "environment",
                    component = "container",
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

    async fn read_stream(&self, path: &Path, mut writer: Writer) -> EnvResult<()> {
        let path = self.local_path(path);
        let file_path = self.path.join(&path);
        ensure!(
            file_path.exists(),
            error::NotFoundSnafu {
                path: path.to_path_buf()
            }
        );
        if file_path.is_file() {
            trace!(
                subsystem = "environment",
                component = "container",
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
                component = "container",
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

    async fn read_bytes(&self, path: &Path) -> EnvResult<Vec<u8>> {
        let path = self.local_path(path);
        let file_path = self.path.join(&path);
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
        trace!(
            subsystem = "environment",
            component = "container",
            op = "exec",
            path = %work_dir.display(),
            "running command"
        );
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
            "container-exec",
            subsystem = "environment",
            component = "container",
            id = %id
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
