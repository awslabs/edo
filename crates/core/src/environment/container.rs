use async_trait::async_trait;
use dashmap::DashMap;
use regex::Regex;
use snafu::ResultExt;
use snafu::{OptionExt, ensure};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::fs::{File, create_dir_all};
use which::which;

use edo::{
    context::{Addr, Context, Element, FromElement, Log},
    environment::{EnvResult, Environment, EnvironmentImpl, FarmImpl},
    record,
    source::Source,
    storage::{Id, MediaType, Storage},
    util::{Reader, Writer, cmd_collect_out, cmd_noinput, cmd_noredirect, cmd_nulled, from_dash},
};

static SHA256_EXTRACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("sha256:[a-fA-F0-9]{64}").expect("the image digest regex is valid")
});

/// Accepts image IDs and named references emitted by supported runtimes.
fn loaded_image_reference(output: &str) -> Option<&str> {
    SHA256_EXTRACT
        .find(output)
        .map(|matched| matched.as_str())
        .or_else(|| {
            output.lines().find_map(|line| {
                ["Loaded image: ", "Loaded image(s): ", "Loaded image ID: "]
                    .into_iter()
                    .find_map(|prefix| line.trim().strip_prefix(prefix))
                    .map(str::trim)
                    .filter(|reference| !reference.is_empty())
            })
        })
}

/// Configuration for the container runtime (e.g. which CLI binary to use).
///
/// Detects `podman`, `finch`, or `docker` when no runtime is configured.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ContainerConfig {
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    cli: Option<PathBuf>,
    #[serde(default)]
    network: bool,
    /// `--pids-limit`: -1 (default) is unlimited; 0 uses the runtime default.
    #[serde(default = "default_pids_limit")]
    pids_limit: Option<i64>,
    /// Repeated `--ulimit` values in `name=soft[:hard]` format.
    #[serde(default)]
    ulimits: Vec<String>,
    /// Run unprivileged commands in `user = "root"` environments as the host
    /// user's `uid:gid` (with `HOME=/root`) rather than as root; only
    /// [`EnvironmentImpl::execute_privileged`] runs as root. `None` (the
    /// default) enables it for rootful runtimes.
    #[serde(default)]
    map_host_user: Option<bool>,
}

fn default_pids_limit() -> Option<i64> {
    Some(-1)
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            runtime: None,
            cli: None,
            network: false,
            pids_limit: default_pids_limit(),
            ulimits: Vec::new(),
            map_host_user: None,
        }
    }
}

const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

static ROOTFUL_CACHE: LazyLock<DashMap<(String, PathBuf), bool>> = LazyLock::new(DashMap::new);

/// Whether `runtime` runs containers with real root privileges, in which case
/// files created by container root on a bind mount are owned by host root.
///
/// Detection failures are treated as rootless. Only Linux hosts talking to a
/// local daemon can be rootful.
async fn runtime_is_rootful(runtime: &str, cli: &Path) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let key = (runtime.to_string(), cli.to_path_buf());
    if let Some(rootful) = ROOTFUL_CACHE.get(&key) {
        return *rootful;
    }
    let rootful = match runtime {
        "docker" | "finch" => probe_info(cli, DOCKER_INFO_FORMAT)
            .await
            .is_some_and(|x| docker_info_is_rootful(&x)),
        "podman" => probe_info(cli, PODMAN_INFO_FORMAT)
            .await
            .is_some_and(|x| podman_info_is_rootful(&x)),
        _ => false,
    };
    ROOTFUL_CACHE.insert(key, rootful);
    rootful
}

/// Run `<cli> info --format <format>`, returning its trimmed stdout on success.
async fn probe_info(cli: &Path, format: &str) -> Option<String> {
    let output = tokio::time::timeout(
        RUNTIME_PROBE_TIMEOUT,
        tokio::process::Command::new(cli)
            .args(["info", "--format", format])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

const DOCKER_INFO_FORMAT: &str = "{{.OperatingSystem}}\t{{json .SecurityOptions}}";

/// Parse [`DOCKER_INFO_FORMAT`] output. Docker Desktop runs its daemon in a VM
/// whose file sharing already maps ownership to the host user.
fn docker_info_is_rootful(info: &str) -> bool {
    let Some((os, security)) = info.split_once('\t') else {
        return false;
    };
    os != "Docker Desktop" && !security.contains("rootless")
}

const PODMAN_INFO_FORMAT: &str = "{{.Host.Security.Rootless}}\t{{.Host.ServiceIsRemote}}";

/// Parse [`PODMAN_INFO_FORMAT`] output. A remote service (e.g. `podman
/// machine`) shares host directories through a VM that maps ownership.
fn podman_info_is_rootful(info: &str) -> bool {
    info.split_once('\t')
        .is_some_and(|(rootless, remote)| rootless == "false" && remote == "false")
}

/// Options for a Container environment
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct ContainerOptions {
    #[serde(default = "default_user")]
    user: String,
    runtime: Option<String>,
    cli: Option<PathBuf>,
    network: Option<bool>,
}

fn default_user() -> String {
    "root".into()
}

/// Runtime settings validated once before creating a farm or environment.
#[derive(Debug, Clone)]
struct ResolvedContainerConfig {
    runtime: String,
    cli: PathBuf,
    network: bool,
    pids_limit: Option<i64>,
    ulimits: Vec<String>,
    /// Left unset by [`ContainerConfig::resolve`] when autodetection is
    /// needed; the farm fills it in before creating environments.
    map_host_user: Option<bool>,
}

impl ContainerConfig {
    fn resolve(
        mut self,
        options: &ContainerOptions,
        mut find: impl FnMut(&str) -> Option<PathBuf>,
    ) -> Result<ResolvedContainerConfig, error::Error> {
        if let Some(runtime) = options.runtime.as_ref() {
            if self.runtime.as_ref() != Some(runtime) {
                self.cli = None;
            }
            self.runtime = Some(runtime.clone());
        }
        self.cli = options.cli.clone().or(self.cli);
        self.network = options.network.unwrap_or(self.network);
        let runtime = if let Some(runtime) = self.runtime {
            if self.cli.is_none() {
                self.cli = Some(find(&runtime).context(error::NoRuntimeSnafu)?);
            }
            runtime
        } else {
            let (runtime, cli) = ["podman", "finch", "docker"]
                .into_iter()
                .find_map(|runtime| find(runtime).map(|cli| (runtime, cli)))
                .context(error::NoRuntimeSnafu)?;
            self.cli = self.cli.or(Some(cli));
            runtime.to_string()
        };
        Ok(ResolvedContainerConfig {
            runtime,
            cli: self.cli.context(error::NoRuntimeSnafu)?,
            network: self.network,
            pids_limit: self.pids_limit.filter(|limit| *limit != 0),
            ulimits: self.ulimits,
            map_host_user: self.map_host_user,
        })
    }
}

/// Creates environments using a container runtime.
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
        let config: ContainerConfig = if let Some(config) = ctx.config().get("container") {
            serde_json::from_value(config).context(error::ConfigSnafu {
                addr: element.addr.clone(),
            })?
        } else {
            ContainerConfig::default()
        };
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

impl ContainerFarm {
    /// Detects an execution runtime only when setup or creation needs it.
    async fn resolve_config(&self) -> Result<ResolvedContainerConfig, error::Error> {
        let mut config = self.config.clone().resolve(&self.options, |runtime| {
            let cli = which(runtime).ok()?;
            edo::ui_info!(
                subsystem = "environment",
                component = "container",
                op = "runtime-detect";
                "found container runtime {runtime} at {cli:?}"
            );
            Some(cli)
        })?;
        if config.map_host_user.is_none() {
            let rootful = runtime_is_rootful(&config.runtime, &config.cli).await;
            debug!(
                subsystem = "environment",
                component = "container",
                op = "runtime-detect",
                rootful = rootful,
                "detected container runtime privilege mode"
            );
            config.map_host_user = Some(rootful);
        }
        Ok(config)
    }
}

#[async_trait]
impl FarmImpl for ContainerFarm {
    async fn fetch(&self, log: &Log, storage: &Storage) -> EnvResult<()> {
        self.source
            .cache(log, storage)
            .await
            .context(error::SourceSnafu)?;
        Ok(())
    }

    #[tracing::instrument(name = "container-load-image", skip_all, fields(addr = %self.addr, id))]
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()> {
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

        let name = format!(
            "edo-{}",
            self.addr
                .to_string()
                .strip_prefix("//")
                .unwrap_or(self.addr.to_string().as_str())
                .replace('/', "-")
        );
        // Reuse an image already loaded in the runtime.
        trace!(
            subsystem = "environment",
            component = "container",
            "check if the image is already loaded into the container runtime"
        );
        let config = self.resolve_config().await?;
        let cli = &config.cli;
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
        // The first source layer is an OCI archive.
        let layer = artifact.layers().first().unwrap();
        let mut reader = storage.safe_read(layer).await?;

        let archive_path = NamedTempFile::new()
            .context(error::IoSnafu)?
            .into_temp_path();
        let path: &Path = archive_path.as_ref();
        let mut archive = File::create(path).await.context(error::IoSnafu)?;
        tokio::io::copy(&mut reader, &mut archive)
            .await
            .context(error::IoSnafu)?;
        drop(archive);

        let artifact_id = artifact.config().id().to_string();
        tracing::Span::current().record("id", &artifact_id);
        // Load and tag the image.
        record!(log, "load_image", "{:?} load -i {path:?}", cli);
        let output = cmd_collect_out(
            ".",
            log,
            cli,
            ["load", "-i", path.to_str().unwrap()],
            &HashMap::new(),
        )
        .context(error::LoadSnafu { log: log.path() })?;
        let string = String::from_utf8_lossy(output.as_slice());
        let digest = loaded_image_reference(&string).context(error::LoadNoDigestSnafu {
            output: string.as_ref(),
        })?;
        record!(log, "tag_image", "{:?} tag {} {name}", cli, digest);
        let tag_success = cmd_noinput(
            ".",
            log,
            cli,
            ["tag", digest, name.as_str()],
            &HashMap::new(),
        )
        .context(error::RuntimeSnafu)?;
        ensure!(tag_success, error::TagFailedSnafu { digest, tag: name });
        edo::ui_info!(
            subsystem = "environment",
            component = "container",
            op = "image-load",
            id = artifact_id;
            "image loaded into container runtime"
        );
        archive_path.close().context(error::IoSnafu)?;
        Ok(())
    }

    async fn create(&self, _log: &Log, path: &Path) -> EnvResult<Environment> {
        trace!(
            subsystem = "environment",
            component = "container",
            path = %path.display(),
            "creating new container environment"
        );
        let config = self.resolve_config().await?;
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
            config,
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
    config: ResolvedContainerConfig,
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
    /// Whether unprivileged commands run as the host user instead of root.
    fn maps_host_user(&self) -> bool {
        self.user == "root" && self.config.map_host_user.unwrap_or(false)
    }

    /// The host `uid`/`gid` owning the workspace, which edo itself created.
    fn host_ids(&self) -> EnvResult<(u32, u32)> {
        let metadata = std::fs::metadata(&self.path).context(error::WorkspaceOwnerSnafu {
            path: self.path.clone(),
        })?;
        Ok((metadata.uid(), metadata.gid()))
    }

    /// `docker exec` arguments selecting the user a command runs as.
    fn user_args(&self, privileged: bool) -> EnvResult<Vec<String>> {
        if privileged || (self.user == "root" && !self.maps_host_user()) {
            return Ok(vec!["-u".into(), "0:0".into()]);
        }
        if self.user != "root" {
            return Ok(Vec::new());
        }
        let (uid, gid) = self.host_ids()?;
        Ok(vec![
            "-u".into(),
            format!("{uid}:{gid}"),
            "--env".into(),
            "HOME=/root".into(),
        ])
    }

    /// The container path the workspace is mounted at.
    fn home(&self) -> String {
        if self.user == "root" {
            "/root".into()
        } else {
            format!("/home/{}", self.user)
        }
    }

    /// The `chown` owner to restore the workspace to after a privileged
    /// command, or `None` when unprivileged commands run as root anyway.
    fn workspace_owner(&self, privileged: bool) -> EnvResult<Option<String>> {
        if !privileged {
            return Ok(None);
        }
        if self.user != "root" {
            let user = &self.user;
            return Ok(Some(format!("$(id -u {user}):$(id -g {user})")));
        }
        if self.maps_host_user() {
            let (uid, gid) = self.host_ids()?;
            return Ok(Some(format!("{uid}:{gid}")));
        }
        Ok(None)
    }

    async fn exec(&self, log: &Log, path: &Path, cmd: &str, privileged: bool) -> EnvResult<bool> {
        let work_dir = Path::new("/root").join(path);
        trace!(
            subsystem = "environment",
            component = "container",
            op = "exec",
            path = %work_dir.display(),
            privileged = privileged,
            "running command"
        );
        let cli = &self.config.cli;
        // Build commands do not attach stdin.
        let mut args = vec![
            "exec".to_string(),
            "--workdir".to_string(),
            format!("{}", work_dir.display()),
        ];
        args.extend(self.user_args(privileged)?);
        self.push_env_args(&mut args);
        args.push(self.name.clone());
        // A privileged command may leave root-owned files in the workspace;
        // hand them back to whoever runs unprivileged commands.
        let cmd = match self.workspace_owner(privileged)? {
            Some(owner) => privileged_wrapper(cmd, &owner, &self.home()),
            None => cmd.to_string(),
        };
        let mut run_args = args.clone();
        run_args.push("sh".into());
        run_args.push("-c".into());
        run_args.push(cmd);
        record!(log, "exec", "{:?} {}", cli, run_args.join(" "));
        let status = edo::util::cmd_noinput(".", log, cli, run_args, &from_dash(&self.env))
            .context(error::RuntimeSnafu)?;
        Ok(status)
    }

    fn local_path(&self, path: &Path) -> EnvResult<PathBuf> {
        let home = if self.user == "root" {
            PathBuf::from("/root")
        } else {
            PathBuf::from(format!("/home/{}", self.user))
        };
        let relative = path.strip_prefix(home).unwrap_or(path);
        let mut local = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => local.push(part),
                Component::CurDir => {}
                Component::ParentDir if local.pop() => {}
                _ => {
                    return error::OutsideWorkspaceSnafu {
                        path: path.to_path_buf(),
                    }
                    .fail()
                    .map_err(Into::into);
                }
            }
        }
        Ok(local)
    }

    /// Append one `--env KEY=VALUE` pair per environment variable.
    fn push_env_args(&self, args: &mut Vec<String>) {
        for entry in self.env.iter() {
            args.push("--env".into());
            args.push(format!("{}={}", entry.key(), entry.value()));
        }
    }
}

/// Wrap `cmd` so `dir` is chowned to `owner` afterwards. The command runs in a
/// subshell so an `exit` inside it can't skip the chown, and its exit status
/// is preserved.
fn privileged_wrapper(cmd: &str, owner: &str, dir: &str) -> String {
    format!(
        "(\n{cmd}\n); rc=$?; chown -hR {owner} {dir} || echo 'edo: failed to restore workspace ownership' >&2; exit $rc"
    )
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
            "setting environment variable"
        );
        self.env.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn get_env(&self, key: &str) -> Option<String> {
        self.env.get(key).map(|x| x.value().clone())
    }

    async fn setup(&self, log: &Log, _storage: &Storage) -> EnvResult<()> {
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

    #[tracing::instrument(name = "container-up", skip_all, fields(container = %self.name))]
    async fn up(&self, log: &Log) -> EnvResult<()> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let cli = &self.config.cli;
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
        if let Some(limit) = self.config.pids_limit {
            args.push("--pids-limit".to_string());
            args.push(limit.to_string());
        }
        for ulimit in &self.config.ulimits {
            args.push("--ulimit".to_string());
            args.push(ulimit.clone());
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
        self.push_env_args(&mut args);
        args.push("--name".into());
        args.push(self.name.clone());
        args.push(self.tag.clone());
        args.push("sh".into());
        record!(log, "launch", "{:?} {}", cli, args.join(" "));
        let launch_success = edo::util::cmd_noinput(".", log, cli, args, &from_dash(&self.env))
            .context(error::RuntimeSnafu)?;
        ensure!(launch_success, error::LaunchFailedSnafu);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn down(&self, log: &Log) -> EnvResult<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let cli = &self.config.cli;
        record!(log, "clean", "{:?} rm -f {}", cli, self.name);
        let rm_status = edo::util::cmd_noinput(
            ".",
            log,
            cli,
            ["rm", "-f", self.name.as_str()],
            &from_dash(&self.env),
        );
        self.running.store(false, Ordering::SeqCst);
        let rm_status = rm_status.context(error::RuntimeSnafu)?;
        ensure!(
            rm_status,
            error::RemoveFailedSnafu {
                container: self.name.clone()
            }
        );
        Ok(())
    }

    async fn clean(&self, _log: &Log) -> EnvResult<()> {
        Ok(())
    }

    async fn create_dir(&self, path: &Path) -> EnvResult<()> {
        let path = self.local_path(path)?;
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
        let path = self.local_path(path)?;
        let file_path = self.path.join(&path);
        if let Some(parent) = file_path.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .context(error::CreateDirectorySnafu)?;
        }
        trace!(
            subsystem = "environment",
            component = "container",
            op = "write-file",
            path = %file_path.display(),
            "writing contents to file"
        );
        tokio::fs::write(&file_path, buffer)
            .await
            .context(error::WriteFileSnafu)?;
        Ok(())
    }

    async fn write_stream(&self, path: &Path, mut reader: Reader) -> EnvResult<()> {
        let path = self.local_path(path)?;
        let file_path = self.path.join(&path);
        if let Some(parent) = file_path.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .context(error::CreateDirectorySnafu)?;
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
        let path = self.local_path(path)?;
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
                edo::util::archive::unpack(reader, &file_path, true)
                    .await
                    .context(error::ExtractSnafu)?;
            }
        }
        Ok(())
    }

    async fn read_stream(&self, path: &Path, mut writer: Writer) -> EnvResult<()> {
        let file_path = self.path.join(self.local_path(path)?);
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
            edo::util::archive::pack_dir(writer, &file_path)
                .await
                .context(error::ArchiveSnafu)?;
        }
        Ok(())
    }

    async fn read_bytes(&self, path: &Path) -> EnvResult<Vec<u8>> {
        let file_path = self.path.join(self.local_path(path)?);
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
        let cli = &self.config.cli;
        let mut args = vec![
            "exec".to_string(),
            "-it".to_string(),
            "--workdir".to_string(),
            format!("{}", work_dir.display()),
        ];
        args.extend(self.user_args(false)?);
        self.push_env_args(&mut args);
        args.push(self.name.clone());
        let mut run_args = args.clone();
        run_args.push("sh".into());
        // Note: we explicitly ignore the cmd status here because the user can influence the return/exit code of this process when
        // exiting the spawned shell
        cmd_noredirect(".", cli, run_args, &from_dash(&self.env)).context(error::RuntimeSnafu)?;
        Ok(())
    }

    #[tracing::instrument(name = "container-exec", skip_all, fields(id = %id, container = %self.name))]
    async fn execute(&self, log: &Log, id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        self.exec(log, path, cmd, false).await
    }

    #[tracing::instrument(name = "container-exec-privileged", skip_all, fields(id = %id, container = %self.name))]
    async fn execute_privileged(
        &self,
        log: &Log,
        id: &Id,
        path: &Path,
        cmd: &str,
    ) -> EnvResult<bool> {
        self.exec(log, path, cmd, true).await
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
        #[snafu(display("container runtime failed to launch container"))]
        LaunchFailed,
        #[snafu(display("failed to load OCI image into container runtime; see {}: {source}", log.display()))]
        Load {
            source: std::io::Error,
            log: PathBuf,
        },
        #[snafu(display(
            "container runtime image load did not return an image ID or reference: {output}"
        ))]
        LoadNoDigest { output: String },
        #[snafu(display(
            "no supported container runtime was found, make sure one of podman, finch or docker is available"
        ))]
        NoRuntime,
        #[snafu(display("container environments must have a source"))]
        NoSource,
        #[snafu(display("file does not exist: {}", path.display()))]
        NotFound { path: PathBuf },
        #[snafu(display("path is outside the container workspace: {}", path.display()))]
        OutsideWorkspace { path: PathBuf },
        #[snafu(display("failed to read file: {source}"))]
        ReadFile { source: std::io::Error },
        #[snafu(display("container runtime failed to remove container {container}"))]
        RemoveFailed { container: String },
        #[snafu(display("failed to execute runtime: {source}"))]
        Runtime { source: std::io::Error },
        #[snafu(display("{source}"))]
        Source {
            #[snafu(source(from(edo::source::SourceError, Box::new)))]
            source: Box<edo::source::SourceError>,
        },
        #[snafu(display("container runtime failed to stop container {container}"))]
        StopFailed { container: String },
        #[snafu(display("{source}"))]
        Storage {
            #[snafu(source(from(edo::storage::StorageError, Box::new)))]
            source: Box<edo::storage::StorageError>,
        },
        #[snafu(display("failed to tag image with digest {digest} as {tag}"))]
        TagFailed { digest: String, tag: String },
        #[snafu(display("artifact does not have an image tag in its metadata"))]
        TagMissing,
        #[snafu(display("failed to create workspace directory: {source}"))]
        Workspace { source: std::io::Error },
        #[snafu(display("failed to stat workspace {}: {source}", path.display()))]
        WorkspaceOwner {
            path: PathBuf,
            source: std::io::Error,
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_loaded_image_reference_accepts_runtime_output_shapes() {
        let digest = format!("sha256:{}", "aB12".repeat(16));
        for output in [
            format!("Loaded image: {digest}\n"),
            format!("Loaded image ID: {digest}\n"),
            format!("unpacking image\nLoaded image(s): {digest}\n"),
        ] {
            assert_eq!(loaded_image_reference(&output), Some(digest.as_str()));
        }
        for (output, expected) in [
            ("Loaded image: repo:tag\n", "repo:tag"),
            ("Loaded image: localhost/foo:tag\n", "localhost/foo:tag"),
            (
                "Loaded image(s): registry.example/foo:tag\n",
                "registry.example/foo:tag",
            ),
        ] {
            assert_eq!(loaded_image_reference(output), Some(expected));
        }
        for output in ["", "unpacking image", "Loaded image: \n"] {
            assert!(
                loaded_image_reference(output).is_none(),
                "unexpected reference in {output:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_farm_fetch_and_container_failures_do_not_leak_resources() {
        use edo::context::{EventLog, LogVerbosity, SourceMap};
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let ctx = Context::init::<&Path, &Path>(
            Some(dir.path()),
            None,
            HashMap::new(),
            LogVerbosity::Info,
            EventLog::Disabled,
        )
        .await
        .unwrap();
        let source_path = dir.path().join("image.tar");
        std::fs::write(&source_path, b"test image archive").unwrap();
        crate::register_core(&ctx);
        let source_element = Element::builder()
            .addr(Addr::parse("//test/archive").unwrap())
            .kind("local")
            .config([("path".to_string(), json!(source_path))])
            .build();
        let farm_element = Element::builder()
            .addr(Addr::parse("//test/image").unwrap())
            .kind("container")
            .source(SourceMap::Resolved(BTreeMap::from([(
                "default".into(),
                vec![source_element],
            )])))
            .config([("runtime".to_string(), json!("edo-nonexistent-runtime"))])
            .build();
        let farm = ContainerFarm::new(&farm_element, &ctx)
            .await
            .expect("loading a farm must not detect a runtime");
        let id = farm.source.get_unique_id().await.unwrap();
        let log = ctx.log().create("container-review").await.unwrap();
        farm.fetch(&log, ctx.storage())
            .await
            .expect("prefetch must not detect a runtime");
        assert!(
            ctx.storage().has_local(&id).await.unwrap(),
            "farm image must be cached"
        );
        assert!(
            farm.resolve_config().await.is_err(),
            "execution still requires a runtime"
        );

        let cli = dir.path().join("runtime");
        let archive_record = dir.path().join("archive-path");
        let calls = dir.path().join("calls");
        let mut farm = farm;
        farm.options = options(json!({"cli": cli, "runtime": "fake"}));
        for (load_output, load_exit, tag_exit, expected) in [
            ("Loaded image: repo:tag", 7, 0, "load"),
            ("unrecognized output", 0, 0, "parse"),
            ("Loaded image: repo:tag", 0, 8, "tag"),
            ("Loaded image: localhost/foo:tag", 0, 0, "success"),
        ] {
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n image) exit 1 ;;\n load) printf '%s' \"$3\" > '{}'; printf '%s\\n' '{load_output}'; echo load-diagnostic >&2; exit {load_exit} ;;\n tag) exit {tag_exit} ;;\n rm) exit 0 ;;\n kill) exit 9 ;;\nesac\n",
                calls.display(),
                archive_record.display()
            );
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let result = farm.setup(&log, ctx.storage()).await;
            let implementation = result.as_ref().err().and_then(|err| match err {
                edo::environment::error::EnvironmentError::Implementation { source } => {
                    source.downcast_ref::<error::Error>()
                }
                _ => None,
            });
            match expected {
                "load" => assert!(
                    matches!(implementation, Some(error::Error::Load { .. })),
                    "unexpected result: {result:?}"
                ),
                "parse" => assert!(
                    matches!(implementation, Some(error::Error::LoadNoDigest { .. })),
                    "unexpected result: {result:?}"
                ),
                "tag" => assert!(
                    matches!(implementation, Some(error::Error::TagFailed { .. })),
                    "unexpected result: {result:?}"
                ),
                "success" => assert!(result.is_ok(), "unexpected result: {result:?}"),
                _ => unreachable!("all cases are specified above"),
            }
            let archive = std::fs::read_to_string(&archive_record).unwrap();
            assert!(
                !Path::new(&archive).exists(),
                "image archive leaked after {expected}"
            );
        }
        let logged = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            logged.contains("load-diagnostic"),
            "load stderr belongs in the log"
        );

        let mut container = container("root", false, dir.path());
        container.config.cli = cli.clone();
        container.running.store(true, Ordering::SeqCst);
        container
            .down(&log)
            .await
            .expect("force-remove covers an already stopped container");
        assert!(
            !container.running.load(Ordering::SeqCst),
            "cleanup must clear running"
        );
        let invoked = std::fs::read_to_string(&calls).unwrap();
        assert!(
            invoked.contains("rm -f test"),
            "container must be force-removed"
        );
        assert!(
            !invoked.lines().any(|line| line.starts_with("kill ")),
            "cleanup must not depend on kill succeeding"
        );

        std::fs::write(&cli, "#!/bin/sh\nexit 9\n").unwrap();
        container.running.store(true, Ordering::SeqCst);
        assert!(
            container.down(&log).await.is_err(),
            "removal failure must be reported"
        );
        assert!(
            !container.running.load(Ordering::SeqCst),
            "failed cleanup must clear running too"
        );
    }

    fn options(value: serde_json::Value) -> ContainerOptions {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn env_vars_are_passed_as_separate_flags() {
        let container = container("root", false, Path::new("workspace"));
        container.env.insert("A".into(), "1".into());
        container.env.insert("B".into(), "x,y=z".into());
        let mut args = Vec::new();
        container.push_env_args(&mut args);
        assert_eq!(args.len(), 4);
        let mut pairs: Vec<_> = args
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "--env");
                pair[1].clone()
            })
            .collect();
        pairs.sort();
        assert_eq!(pairs, ["A=1", "B=x,y=z"]);
    }

    #[test]
    fn local_paths_remain_in_the_workspace() {
        for (user, home) in [("root", "/root"), ("builder", "/home/builder")] {
            let container = container(user, false, Path::new("workspace"));
            for (path, expected) in [
                (home.to_string(), ""),
                (format!("{home}/src/file"), "src/file"),
                ("src/file".into(), "src/file"),
                ("src/../file".into(), "file"),
                (".".into(), ""),
            ] {
                assert_eq!(
                    container.local_path(Path::new(&path)).unwrap(),
                    Path::new(expected)
                );
            }
            for path in [
                "/opt/out".to_string(),
                "/usr/local/bin".into(),
                "/etc/foo".into(),
                "../out".into(),
                "src/../../out".into(),
                format!("{home}/../out"),
                format!("{home}ish/out"),
            ] {
                assert!(container.local_path(Path::new(&path)).is_err(), "{path}");
            }
        }
        assert!(
            container("builder", false, Path::new("workspace"))
                .local_path(Path::new("/root/out"))
                .is_err()
        );
        assert!(
            container("root", false, Path::new("workspace"))
                .local_path(Path::new("/home/root/out"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn file_operations_map_home_paths_and_preserve_error_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let container = container("builder", false, workspace.path());
        container
            .create_dir(Path::new("/home/builder/src"))
            .await
            .unwrap();
        container
            .write_bytes(Path::new("/home/builder/src/file"), b"data")
            .await
            .unwrap();
        assert_eq!(
            container.read_bytes(Path::new("src/file")).await.unwrap(),
            b"data"
        );
        let error = container
            .read_bytes(Path::new("/home/builder/missing"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("/home/builder/missing"));
    }

    #[test]
    fn user_defaults_to_root() {
        assert_eq!(options(json!({})).user, "root");
        assert_eq!(options(json!({"user": "builder"})).user, "builder");
    }

    #[test]
    fn resource_limits_survive_resolution() {
        for config in [
            ContainerConfig::default(),
            serde_json::from_value(json!({})).unwrap(),
        ] {
            let resolved = config
                .resolve(&options(json!({})), |runtime| Some(PathBuf::from(runtime)))
                .unwrap();
            assert_eq!(resolved.pids_limit, Some(-1));
            assert!(resolved.ulimits.is_empty());
        }
        for limit in [json!(-1), json!(0), json!(4096), json!(null)] {
            let config: ContainerConfig = serde_json::from_value(json!({
                "pids_limit": limit,
                "ulimits": ["nofile=65536:65536"]
            }))
            .unwrap();
            let resolved = config
                .resolve(&options(json!({})), |runtime| Some(PathBuf::from(runtime)))
                .unwrap();
            assert_eq!(
                resolved.pids_limit,
                limit.as_i64().filter(|limit| *limit != 0)
            );
            assert_eq!(resolved.ulimits, ["nofile=65536:65536"]);
        }
    }

    #[test]
    fn toml_can_use_runtime_default_pids_limit() {
        for (input, expected) in [
            ("", Some(-1)),
            ("pids_limit = 0", None),
            ("pids_limit = 4096", Some(4096)),
        ] {
            let config: ContainerConfig = toml::from_str(input).unwrap();
            let resolved = config
                .resolve(&options(json!({})), |runtime| Some(PathBuf::from(runtime)))
                .unwrap();
            assert_eq!(resolved.pids_limit, expected);
        }
    }

    #[test]
    fn partial_config_keeps_network_during_detection() {
        for (global, element) in [
            (json!({"network": true}), json!({})),
            (json!({}), json!({"network": true})),
        ] {
            let config: ContainerConfig = serde_json::from_value(global).unwrap();
            let config = config
                .resolve(&options(element), |runtime| Some(PathBuf::from(runtime)))
                .unwrap();
            assert_eq!(config.cli, PathBuf::from("podman"));
            assert!(config.network);
        }
    }

    #[test]
    fn element_overrides_global_settings() {
        let global: ContainerConfig = serde_json::from_value(
            json!({"runtime": "docker", "cli": "/custom/docker", "network": true}),
        )
        .unwrap();
        let config = global
            .clone()
            .resolve(&options(json!({"network": false})), |_| {
                panic!("configured executable should be retained")
            })
            .unwrap();
        assert_eq!(config.cli, PathBuf::from("/custom/docker"));
        assert!(!config.network);
        let config = global
            .resolve(&options(json!({"runtime": "podman"})), |runtime| {
                Some(PathBuf::from(runtime))
            })
            .unwrap();
        assert_eq!(config.cli, PathBuf::from("podman"));
        assert!(config.network);
    }

    #[test]
    fn explicit_cli_overrides_lookup() {
        let config = ContainerConfig::default()
            .resolve(
                &options(json!({"runtime": "docker", "cli": "/custom/docker"})),
                |_| panic!("explicit executable should not be looked up"),
            )
            .unwrap();
        assert_eq!(config.cli, PathBuf::from("/custom/docker"));
    }

    #[test]
    fn detection_checks_podman_then_finch_then_docker() {
        let mut attempts = Vec::new();
        let config = ContainerConfig::default()
            .resolve(&options(json!({})), |runtime| {
                attempts.push(runtime.to_string());
                (runtime == "docker").then(|| PathBuf::from("docker"))
            })
            .unwrap();
        assert_eq!(attempts, ["podman", "finch", "docker"]);
        assert_eq!(config.cli, PathBuf::from("docker"));
        assert!(
            ContainerConfig::default()
                .resolve(&options(json!({})), |_| None)
                .is_err()
        );
    }

    #[test]
    fn configured_runtime_must_resolve_before_construction() {
        let mut attempts = Vec::new();
        let result =
            ContainerConfig::default().resolve(&options(json!({"runtime": "podman"})), |runtime| {
                attempts.push(runtime.to_string());
                None
            });
        assert!(matches!(result, Err(error::Error::NoRuntime)));
        assert_eq!(attempts, ["podman"]);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        for value in [
            json!({"runtime": "podman", "cli": 5}),
            json!({"runtme": "podman"}),
            json!({"network": "true"}),
        ] {
            assert!(serde_json::from_value::<ContainerOptions>(value.clone()).is_err());
            assert!(serde_json::from_value::<ContainerConfig>(value).is_err());
        }
    }

    fn container(user: &str, map_host_user: bool, path: &Path) -> Container {
        Container {
            config: ResolvedContainerConfig {
                runtime: "podman".into(),
                cli: "podman".into(),
                network: false,
                pids_limit: None,
                ulimits: Vec::new(),
                map_host_user: Some(map_host_user),
            },
            name: "test".into(),
            user: user.into(),
            path: path.to_path_buf(),
            tag: "test".into(),
            running: AtomicBool::new(false),
            env: DashMap::new(),
        }
    }

    fn root_args() -> Vec<String> {
        vec!["-u".into(), "0:0".into()]
    }

    #[test]
    fn user_args_decision_table() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(dir.path()).unwrap();
        let host = format!("{}:{}", metadata.uid(), metadata.gid());

        let root = container("root", false, dir.path());
        assert_eq!(root.user_args(false).unwrap(), root_args());
        assert_eq!(root.user_args(true).unwrap(), root_args());

        let mapped = container("root", true, dir.path());
        assert_eq!(
            mapped.user_args(false).unwrap(),
            vec!["-u".to_string(), host, "--env".into(), "HOME=/root".into()]
        );
        assert_eq!(mapped.user_args(true).unwrap(), root_args());

        for map_host_user in [false, true] {
            let user = container("builder", map_host_user, dir.path());
            assert!(user.user_args(false).unwrap().is_empty());
            assert_eq!(user.user_args(true).unwrap(), root_args());
        }
    }

    #[test]
    fn workspace_owner_decision_table() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(dir.path()).unwrap();
        let host = format!("{}:{}", metadata.uid(), metadata.gid());

        for (user, map_host_user) in [("root", false), ("root", true), ("builder", true)] {
            let env = container(user, map_host_user, dir.path());
            assert_eq!(env.workspace_owner(false).unwrap(), None);
        }
        assert_eq!(
            container("root", false, dir.path())
                .workspace_owner(true)
                .unwrap(),
            None
        );
        assert_eq!(
            container("root", true, dir.path())
                .workspace_owner(true)
                .unwrap(),
            Some(host)
        );
        assert_eq!(
            container("builder", false, dir.path())
                .workspace_owner(true)
                .unwrap(),
            Some("$(id -u builder):$(id -g builder)".into())
        );
    }

    #[test]
    fn host_ids_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let env = container("root", true, &dir.path().join("missing"));
        let err = env.user_args(false).unwrap_err().to_string();
        assert!(err.contains("failed to stat workspace"), "{err}");
    }

    fn run_wrapper(cmd: &str, dir: &Path) -> std::process::Output {
        let id = |flag| {
            let out = std::process::Command::new("id").arg(flag).output().unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let owner = format!("{}:{}", id("-u"), id("-g"));
        std::process::Command::new("sh")
            .arg("-c")
            .arg(privileged_wrapper(cmd, &owner, dir.to_str().unwrap()))
            .output()
            .unwrap()
    }

    #[test]
    fn privileged_wrapper_preserves_status() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_wrapper("true", dir.path()).status.code(), Some(0));
        assert_eq!(run_wrapper("false", dir.path()).status.code(), Some(1));
        let out = run_wrapper("echo hi\nexit 3", dir.path());
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    }

    #[test]
    fn privileged_wrapper_chowns_after_exit() {
        // A missing directory makes the chown observable through its error.
        let dir = tempfile::tempdir().unwrap();
        let out = run_wrapper("exit 4", &dir.path().join("missing"));
        assert_eq!(out.status.code(), Some(4));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("edo: failed to restore workspace ownership")
        );
    }

    #[test]
    fn docker_info_detection() {
        assert!(docker_info_is_rootful(
            "Fedora Linux 44\t[\"name=seccomp,profile=builtin\",\"name=selinux\"]"
        ));
        assert!(!docker_info_is_rootful(
            "Fedora Linux 44\t[\"name=seccomp,profile=builtin\",\"name=rootless\"]"
        ));
        assert!(!docker_info_is_rootful(
            "Docker Desktop\t[\"name=seccomp,profile=builtin\"]"
        ));
        assert!(!docker_info_is_rootful(""));
    }

    #[test]
    fn podman_info_detection() {
        assert!(podman_info_is_rootful("false\tfalse"));
        assert!(!podman_info_is_rootful("true\tfalse"));
        assert!(!podman_info_is_rootful("false\ttrue"));
        assert!(!podman_info_is_rootful("false"));
        assert!(!podman_info_is_rootful(""));
    }
}
