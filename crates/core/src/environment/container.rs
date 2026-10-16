use async_trait::async_trait;
use dashmap::DashMap;
use regex::Regex;
use snafu::ResultExt;
use snafu::{OptionExt, ensure};
use std::collections::HashMap;
use std::env;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::fs::{File, create_dir_all, remove_file};
use uuid::Uuid;
use which::which;

use edo::{
    context::{Addr, Context, Element, FromElement, Log},
    environment::{EnvResult, Environment, EnvironmentImpl, FarmImpl},
    record,
    source::Source,
    storage::{Id, MediaType, Storage},
    util::{Reader, Writer, cmd_collect_out, cmd_noinput, cmd_noredirect, cmd_nulled, from_dash},
};

const SHA256_EXTRACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("sha256:([a-fA-F0-9]{64})").unwrap());

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
    pids_limit: Option<i64>,
    /// Per-container ulimits, each formatted as `name=soft[:hard]` and passed
    /// through verbatim as repeated `--ulimit` flags (e.g. `"nproc=65535"`,
    /// `"nofile=1048576"`). Defaults to empty (runtime defaults apply).
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
            edo::ui_info!(
                subsystem = "environment",
                component = "container",
                op = "runtime-detect";
                "found container runtime {runtime} at {cli:?}"
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
        if config.map_host_user.is_none() {
            // SAFETY: runtime and cli are Some at this point.
            let rootful = runtime_is_rootful(
                config.runtime.as_deref().unwrap(),
                config.cli.as_ref().unwrap(),
            )
            .await;
            debug!(
                subsystem = "environment",
                component = "container",
                op = "runtime-detect",
                rootful = rootful,
                "detected container runtime privilege mode"
            );
            config.map_host_user = Some(rootful);
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
        let mut reader = storage.safe_read(layer).await?;

        let path = env::temp_dir().join(Uuid::now_v7().to_string());
        let mut archive = File::create(&path).await.context(error::IoSnafu)?;
        tokio::io::copy(&mut reader, &mut archive)
            .await
            .context(error::IoSnafu)?;
        drop(archive);

        let artifact_id = artifact.config().id().to_string();
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
        // Use regex to extract the sha256 digest of the image
        let digest = SHA256_EXTRACT
            .captures(&string)
            .and_then(|x| x.get(0))
            .context(error::LoadNoDigestSnafu {
                output: string.clone(),
            })?;

        let digest = digest.as_str();
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
        remove_file(&path).await.context(error::IoSnafu)?;
        Ok(())
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
        let cli = self.config.cli.as_ref().unwrap();
        // No `-i`: stdin is /dev/null for build commands (see
        // `edo::util::cmd_noinput`), and an attached stdin would make the
        // runtime pump the user's terminal, stealing keystrokes from the
        // scheduler's interactive failure prompt.
        let mut args = vec![
            "exec".to_string(),
            "--workdir".to_string(),
            format!("{}", work_dir.display()),
        ];
        args.extend(self.user_args(privileged)?);
        for entry in self.env.iter() {
            args.push("--env".into());
            args.push(format!("{}={}", entry.key(), entry.value()));
        }
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
        for entry in self.env.iter() {
            args.push("--env".into());
            args.push(format!("{}={}", entry.key(), entry.value()));
        }
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
        let cli = self.config.cli.as_ref().unwrap();
        record!(log, "stop", "{:?} kill {}", cli, self.name);
        let kill_status = edo::util::cmd_noinput(
            ".",
            log,
            cli,
            vec!["kill".into(), self.name.clone()],
            &from_dash(&self.env),
        )
        .context(error::RuntimeSnafu)?;
        ensure!(
            kill_status,
            error::StopFailedSnafu {
                container: self.name.clone()
            }
        );
        record!(log, "clean", "{:?} rm {}", cli, self.name);
        let rm_status = edo::util::cmd_noinput(
            ".",
            log,
            cli,
            vec!["rm".into(), self.name.clone()],
            &from_dash(&self.env),
        )
        .context(error::RuntimeSnafu)?;
        ensure!(
            rm_status,
            error::RemoveFailedSnafu {
                container: self.name.clone()
            }
        );
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
        tokio::fs::write(&file_path, buffer)
            .await
            .context(error::WriteFileSnafu)?;
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
        args.extend(self.user_args(false)?);
        for entry in self.env.iter() {
            args.push("--env".into());
            args.push(format!("{}={}", entry.key(), entry.value()));
        }
        args.push(self.name.clone());
        let mut run_args = args.clone();
        run_args.push("sh".into());
        // Note: we explicitly ignore the cmd status here because the user can influence the return/exit code of this process when
        // exiting the spawned shell
        cmd_noredirect(".", cli, run_args, &from_dash(&self.env)).context(error::RuntimeSnafu)?;
        Ok(())
    }

    async fn execute(&self, log: &Log, _id: &Id, path: &Path, cmd: &str) -> EnvResult<bool> {
        self.exec(log, path, cmd, false).await
    }

    async fn execute_privileged(
        &self,
        log: &Log,
        _id: &Id,
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
        #[snafu(display("failed to load oci image into container runtime: {source}"))]
        Load { source: std::io::Error },
        #[snafu(display("container runtime image load did not return a digest: {output}"))]
        LoadNoDigest { output: String },
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

    fn container(user: &str, map_host_user: bool, path: &Path) -> Container {
        Container {
            config: ContainerConfig {
                map_host_user: Some(map_host_user),
                ..Default::default()
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
