//! Virtual filesystem helper bound to an [`Environment`].
//!
//! [`Vfs`] provides a path-oriented façade over [`Environment::cmd`] so that
//! transforms can perform familiar filesystem operations (mkdir, rm, cp, mv,
//! etc.) inside an environment without caring whether that environment is the
//! host filesystem, a container, or some other sandbox. Each operation is
//! dispatched as a shell command to the underlying environment, letting the
//! environment implementation decide how the command is actually executed.
//!
//! A [`Vfs`] carries an implicit "current directory" (`path`). Relative paths
//! passed to its methods are resolved against this directory via
//! [`Environment::expand`]; absolute paths are used as-is. Use
//! [`Vfs::entry`] to obtain a child [`Vfs`] rooted at a nested path without
//! mutating the original.

use std::path::{Path, PathBuf};

use crate::{
    context::Log,
    environment::{EnvResult, Environment, error},
    storage::Id,
};

/// Filesystem view of an [`Environment`], rooted at a tracked working path.
///
/// Cheap to clone — all fields are already reference-counted handles or small
/// owned values. Clone to capture a snapshot of the current working path; use
/// [`Vfs::entry`] to produce a [`Vfs`] rooted at a child path.
#[derive(Clone)]
pub struct Vfs {
    id: Id,
    env: Environment,
    log: Log,
    path: PathBuf,
}

impl Vfs {
    /// Create a new [`Vfs`] rooted at the environment's default working
    /// directory (an empty path, which [`Environment::expand`] will resolve).
    ///
    /// `id` and `log` are captured so every dispatched command is attributed
    /// to the originating transform and recorded in the right log stream.
    pub async fn new(id: &Id, env: &Environment, log: &Log) -> EnvResult<Self> {
        Ok(Self {
            id: id.clone(),
            env: env.clone(),
            log: log.clone(),
            path: PathBuf::new(),
        })
    }

    /// Return the current working path tracked by this [`Vfs`].
    ///
    /// This is the unexpanded path — callers that need an absolute path inside
    /// the environment should go through [`Environment::expand`].
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Resolve `path` to an absolute path inside the environment.
    ///
    /// Absolute inputs are returned unchanged; relative inputs are joined
    /// against the [`Vfs`]'s current working path and then passed through
    /// [`Environment::expand`] so the environment can apply any sandbox-aware
    /// translation.
    async fn canonicalize(&self, path: impl AsRef<Path>) -> EnvResult<PathBuf> {
        if path.as_ref().is_absolute() {
            Ok(path.as_ref().to_path_buf())
        } else {
            self.env.expand(&self.path.join(path.as_ref())).await
        }
    }

    /// Return a child [`Vfs`] whose working path is `self.path().join(path)`.
    ///
    /// Does not create the directory or otherwise touch the environment — it
    /// simply narrows the logical cursor so subsequent relative operations
    /// resolve beneath `path`.
    pub async fn entry(&self, path: impl AsRef<Path>) -> Self {
        Self {
            id: self.id.clone(),
            env: self.env.clone(),
            log: self.log.clone(),
            path: self.path.join(path.as_ref()),
        }
    }

    /// Check whether `path` exists inside the environment.
    ///
    /// Implemented by dispatching `stat` and returning its exit status — any
    /// non-zero exit is reported as `Ok(false)`.
    pub async fn try_exists(&self, path: impl AsRef<Path>) -> EnvResult<bool> {
        let path = self.canonicalize(path).await?;
        self.env
            .cmd(&self.log, &self.id, &self.path(), &format!("stat {path:?}"))
            .await
    }

    /// Set an environment variable in the underlying [`Environment`] and
    /// record the mutation in the log.
    pub async fn set_env(&self, key: &str, value: &str) -> EnvResult<()> {
        self.log.record("set-env", key)?;
        self.env.set_env(key, value).await
    }

    /// Read an environment variable from the underlying [`Environment`] and
    /// record the access in the log.
    pub async fn get_env(&self, key: &str) -> EnvResult<Option<String>> {
        self.log.record("get-env", key)?;
        Ok(self.env.get_env(key).await)
    }

    /// Create `path` and any missing parents (analogous to
    /// [`tokio::fs::create_dir_all`]).
    ///
    /// Returns a new [`Vfs`] whose working path is the freshly created
    /// directory, so callers can chain further operations scoped to it.
    /// Fails with an [`error::VfsSnafu`] if the `mkdir -p` command reports a
    /// non-zero exit status.
    // tokio::fs::create_dir_all
    pub async fn create_dir(&self, path: impl AsRef<Path>) -> EnvResult<Self> {
        let path = self.canonicalize(path).await?;
        if !self
            .env
            .cmd(
                &self.log,
                &self.id,
                &self.path,
                &format!("mkdir -p {path:?}"),
            )
            .await?
        {
            return error::VfsSnafu { action: "mkdir" }.fail();
        }
        Ok(Self {
            id: self.id.clone(),
            env: self.env.clone(),
            log: self.log.clone(),
            path: path.clone(),
        })
    }

    /// Remove a single file at `path` (analogous to [`tokio::fs::remove_file`]).
    ///
    /// Fails with an [`error::VfsSnafu`] if the `rm` command reports a
    /// non-zero exit status.
    // tokio::fs::remove_file
    pub async fn remove_file(&self, path: impl AsRef<Path>) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        if !self
            .env
            .cmd(&self.log, &self.id, &self.path, &format!("rm {path:?}"))
            .await?
        {
            return error::VfsSnafu { action: "rm" }.fail();
        }
        Ok(())
    }

    /// Recursively remove a directory at `path` (analogous to
    /// [`tokio::fs::remove_dir_all`]).
    ///
    /// Fails with an [`error::VfsSnafu`] if the `rm -r` command reports a
    /// non-zero exit status.
    // tokio::fs::remove_dir_all
    pub async fn remove_dir(&self, path: impl AsRef<Path>) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        if !self
            .env
            .cmd(&self.log, &self.id, &self.path, &format!("rm -r {path:?}"))
            .await?
        {
            return error::VfsSnafu { action: "rmdir" }.fail();
        }
        Ok(())
    }

    /// Recursively copy `from` to `to` (analogous to [`tokio::fs::copy`], but
    /// using `cp -r` so directory trees are supported).
    ///
    /// Both paths are canonicalised against the current working path. Fails
    /// with an [`error::VfsSnafu`] if the `cp` command reports a non-zero
    /// exit status.
    // tokio::fs::copy
    pub async fn copy(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> EnvResult<()> {
        let from = self.canonicalize(from).await?;
        let to = self.canonicalize(to).await?;
        if !self
            .env
            .cmd(
                &self.log,
                &self.id,
                &self.path,
                &format!("cp -r {from:?} {to:?}"),
            )
            .await?
        {
            return error::VfsSnafu { action: "copy" }.fail();
        }
        Ok(())
    }

    /// Rename (move) `from` to `to` (analogous to [`tokio::fs::rename`]).
    ///
    /// Both paths are canonicalised against the current working path. Fails
    /// with an [`error::VfsSnafu`] if the `mv` command reports a non-zero
    /// exit status.
    // tokio::fs::rename
    pub async fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> EnvResult<()> {
        let from = self.canonicalize(from).await?;
        let to = self.canonicalize(to).await?;
        if !self
            .env
            .cmd(
                &self.log,
                &self.id,
                &self.path,
                &format!("mv {from:?} {to:?}"),
            )
            .await?
        {
            return error::VfsSnafu { action: "rename" }.fail();
        }
        Ok(())
    }

    /// Run an arbitrary `program` with `args` in the environment at the
    /// [`Vfs`]'s current working path.
    ///
    /// `action` is a short label used in [`error::VfsSnafu`] if the command
    /// reports a non-zero exit status. Arguments are joined with single
    /// spaces — callers needing shell-quoting must quote themselves.
    pub async fn command<S, A, I>(&self, action: &str, program: S, args: A) -> EnvResult<()>
    where
        S: AsRef<str>,
        A: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        if !self
            .env
            .cmd(
                &self.log,
                &self.id,
                &self.path,
                &format!(
                    "{} {}",
                    program.as_ref(),
                    args.into_iter()
                        .map(|x| x.as_ref().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            )
            .await?
        {
            return error::VfsSnafu { action }.fail();
        }
        Ok(())
    }
}

impl AsRef<str> for Vfs {
    fn as_ref(&self) -> &str {
        self.path.to_str().unwrap()
    }
}

impl AsRef<Path> for Vfs {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`Vfs`].
    //!
    //! Mock [`EnvironmentImpl`] is inlined here, mirroring the per-module
    //! convention used by `command.rs` and `farm.rs` (the crate deliberately
    //! avoids a shared `test_support.rs`). `Vfs` drives `Environment::cmd` and
    //! `Environment::expand` exclusively, so the mock records every `cmd()`
    //! invocation as `(path, command_string)` tuples and exposes hooks for
    //! toggling the exit status and the `expand` prefix.
    use super::*;
    use crate::context::test_support::shared_log_manager;
    use crate::environment::{Command, EnvironmentImpl};
    use crate::environment::error::EnvironmentError;
    use crate::storage::{Id, Storage};
    use crate::util::{Reader, Writer};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Configurable `EnvironmentImpl` used by the VFS tests.
    ///
    /// * `expand_prefix` — when `Some`, `expand(p)` returns `prefix.join(p)`
    ///   after stripping any leading `/`; when `None`, it echoes `p`.
    /// * `cmd_status` — the boolean flag returned from `cmd`.
    /// * `cmds` — shared log of `(path, command_string)` tuples observed by
    ///   `cmd()`, letting tests assert exactly what was dispatched.
    /// * `env_vars` — backing store for `set_env` / `get_env`.
    struct MockEnvImpl {
        expand_prefix: Option<PathBuf>,
        cmd_status: bool,
        cmds: Arc<Mutex<Vec<(PathBuf, String)>>>,
        env_vars: Arc<Mutex<HashMap<String, String>>>,
    }

    impl MockEnvImpl {
        fn new() -> (
            Self,
            Arc<Mutex<Vec<(PathBuf, String)>>>,
            Arc<Mutex<HashMap<String, String>>>,
        ) {
            let cmds = Arc::new(Mutex::new(Vec::new()));
            let env_vars = Arc::new(Mutex::new(HashMap::new()));
            (
                Self {
                    expand_prefix: None,
                    cmd_status: true,
                    cmds: cmds.clone(),
                    env_vars: env_vars.clone(),
                },
                cmds,
                env_vars,
            )
        }

        fn with_prefix(mut self, prefix: PathBuf) -> Self {
            self.expand_prefix = Some(prefix);
            self
        }

        fn with_cmd_status(mut self, status: bool) -> Self {
            self.cmd_status = status;
            self
        }
    }

    #[async_trait]
    impl EnvironmentImpl for MockEnvImpl {
        async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
            if let Some(prefix) = &self.expand_prefix {
                let rel = path.strip_prefix("/").unwrap_or(path);
                Ok(prefix.join(rel))
            } else {
                Ok(path.to_path_buf())
            }
        }
        async fn create_dir(&self, _p: &Path) -> EnvResult<()> {
            unimplemented!()
        }
        async fn set_env(&self, k: &str, v: &str) -> EnvResult<()> {
            self.env_vars
                .lock()
                .unwrap()
                .insert(k.to_string(), v.to_string());
            Ok(())
        }
        async fn get_env(&self, k: &str) -> Option<String> {
            self.env_vars.lock().unwrap().get(k).cloned()
        }
        async fn setup(&self, _log: &Log, _storage: &Storage) -> EnvResult<()> {
            unimplemented!()
        }
        async fn up(&self, _log: &Log) -> EnvResult<()> {
            unimplemented!()
        }
        async fn down(&self, _log: &Log) -> EnvResult<()> {
            unimplemented!()
        }
        async fn clean(&self, _log: &Log) -> EnvResult<()> {
            unimplemented!()
        }
        async fn write(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            unimplemented!()
        }
        async fn unpack(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            unimplemented!()
        }
        async fn read(&self, _p: &Path, _w: Writer) -> EnvResult<()> {
            unimplemented!()
        }
        async fn cmd(&self, _log: &Log, _id: &Id, path: &Path, command: &str) -> EnvResult<bool> {
            self.cmds
                .lock()
                .unwrap()
                .push((path.to_path_buf(), command.to_string()));
            Ok(self.cmd_status)
        }
        async fn run(
            &self,
            _log: &Log,
            _id: &Id,
            _p: &Path,
            _c: &Command,
        ) -> EnvResult<bool> {
            unimplemented!()
        }
        fn shell(&self, _p: &Path) -> EnvResult<()> {
            unimplemented!()
        }
    }

    /// Build a fresh `Log` inside `dir` using the process-wide shared
    /// `LogManager`.
    async fn make_log(dir: &TempDir, name: &str) -> Log {
        let mgr = shared_log_manager().await;
        let path = dir.path().join(format!("{name}.log"));
        Log::new(&mgr, &path).expect("Log::new")
    }

    fn make_id() -> Id {
        Id::builder()
            .name("vfs-test".to_string())
            .digest("deadbeef".to_string())
            .build()
    }

    /// Default mock: succeeds, echoes paths.
    fn make_env() -> (
        Environment,
        Arc<Mutex<Vec<(PathBuf, String)>>>,
        Arc<Mutex<HashMap<String, String>>>,
    ) {
        let (mock, cmds, env_vars) = MockEnvImpl::new();
        (Environment::new(mock), cmds, env_vars)
    }

    async fn new_vfs(log: &Log, env: &Environment) -> Vfs {
        let id = make_id();
        Vfs::new(&id, env, log).await.expect("Vfs::new")
    }

    // ── Construction / path ────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn new_starts_with_empty_path() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "new").await;
        let (env, _, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        assert_eq!(vfs.path(), Path::new(""));
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn entry_joins_without_mutating_parent() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "entry").await;
        let (env, _, _) = make_env();
        let parent = new_vfs(&log, &env).await;
        let child = parent.entry("src").await;
        assert_eq!(child.path(), Path::new("src"));
        assert_eq!(parent.path(), Path::new(""));
        let grand = child.entry("build").await;
        assert_eq!(grand.path(), Path::new("src/build"));
    }

    // ── Canonicalisation (observed via dispatched commands) ────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn relative_path_is_joined_with_cwd_then_expanded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rel").await;
        let (mock, cmds, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_prefix(PathBuf::from("/sandbox")));
        let vfs = new_vfs(&log, &env).await.entry("src").await;
        vfs.create_dir("build").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "mkdir -p \"/sandbox/src/build\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn absolute_path_bypasses_cwd_and_expand() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "abs").await;
        let (mock, cmds, _) = MockEnvImpl::new();
        // Even with a prefix, absolute input paths skip `expand`.
        let env = Environment::new(mock.with_prefix(PathBuf::from("/sandbox")));
        let vfs = new_vfs(&log, &env).await.entry("src").await;
        vfs.create_dir("/etc").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "mkdir -p \"/etc\"");
    }

    // ── try_exists ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn try_exists_dispatches_stat_and_returns_true() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "stat-t").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        let ok = vfs.try_exists("/tmp/foo").await.unwrap();
        assert!(ok);
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "stat \"/tmp/foo\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn try_exists_returns_false_when_cmd_fails() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "stat-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let ok = vfs.try_exists("/tmp/missing").await.unwrap();
        assert!(!ok);
    }

    // ── create_dir ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn create_dir_dispatches_mkdir_p_and_returns_child_vfs() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "mkdir").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        let child = vfs.create_dir("/work/out").await.unwrap();
        assert_eq!(child.path(), Path::new("/work/out"));
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "mkdir -p \"/work/out\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn create_dir_failure_returns_vfs_error() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "mkdir-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        // `create_dir` returns `Vfs` on success (no `Debug` impl), so we
        // can't use `unwrap_err`; inspect the `Result` directly instead.
        let err = match vfs.create_dir("/x").await {
            Ok(_) => panic!("expected create_dir to fail"),
            Err(e) => e,
        };
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "mkdir"),
            "expected Vfs{{action=mkdir}}, got {err:?}"
        );
    }

    // ── remove_file ────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn remove_file_dispatches_rm() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rm").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.remove_file("/a/b").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "rm \"/a/b\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn remove_file_failure_returns_vfs_error() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rm-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let err = vfs.remove_file("/a/b").await.unwrap_err();
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "rm"),
            "expected Vfs{{action=rm}}, got {err:?}"
        );
    }

    // ── remove_dir ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn remove_dir_dispatches_rm_r() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rmdir").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.remove_dir("/a/tree").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "rm -r \"/a/tree\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn remove_dir_failure_returns_vfs_error() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rmdir-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let err = vfs.remove_dir("/a/tree").await.unwrap_err();
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "rmdir"),
            "expected Vfs{{action=rmdir}}, got {err:?}"
        );
    }

    // ── copy ───────────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn copy_dispatches_cp_r_with_both_paths() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "cp").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.copy("/src", "/dst").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "cp -r \"/src\" \"/dst\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn copy_failure_returns_vfs_error() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "cp-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let err = vfs.copy("/src", "/dst").await.unwrap_err();
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "copy"),
            "expected Vfs{{action=copy}}, got {err:?}"
        );
    }

    // ── rename ─────────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn rename_dispatches_mv_with_both_paths() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "mv").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.rename("/src", "/dst").await.unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "mv \"/src\" \"/dst\"");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn rename_failure_returns_vfs_error() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "mv-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let err = vfs.rename("/src", "/dst").await.unwrap_err();
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "rename"),
            "expected Vfs{{action=rename}}, got {err:?}"
        );
    }

    // ── command ────────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn command_joins_args_with_spaces() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "cmd").await;
        let (env, cmds, _) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.command("lint", "cargo", ["fmt", "--check"])
            .await
            .unwrap();
        let recorded = cmds.lock().unwrap();
        let (_, cmd) = recorded.last().unwrap();
        assert_eq!(cmd, "cargo fmt --check");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn command_failure_uses_provided_action_label() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "cmd-f").await;
        let (mock, _, _) = MockEnvImpl::new();
        let env = Environment::new(mock.with_cmd_status(false));
        let vfs = new_vfs(&log, &env).await;
        let err = vfs
            .command("lint", "cargo", ["fmt"])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EnvironmentError::Vfs { action } if action == "lint"),
            "expected Vfs{{action=lint}}, got {err:?}"
        );
    }

    // ── env-var passthrough ────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn set_env_delegates_to_environment() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "set-env").await;
        let (env, _, env_vars) = make_env();
        let vfs = new_vfs(&log, &env).await;
        vfs.set_env("KEY", "VAL").await.unwrap();
        let map = env_vars.lock().unwrap();
        assert_eq!(map.get("KEY").map(String::as_str), Some("VAL"));
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn get_env_returns_value_from_environment() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "get-env").await;
        let (env, _, env_vars) = make_env();
        env_vars
            .lock()
            .unwrap()
            .insert("KEY".to_string(), "VAL".to_string());
        let vfs = new_vfs(&log, &env).await;
        let got = vfs.get_env("KEY").await.unwrap();
        assert_eq!(got.as_deref(), Some("VAL"));
        let missing = vfs.get_env("MISSING").await.unwrap();
        assert_eq!(missing, None);
    }

    // ── AsRef impls ────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn as_ref_str_returns_current_path() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "asref-str").await;
        let (env, _, _) = make_env();
        let vfs = new_vfs(&log, &env).await.entry("a/b").await;
        let s: &str = vfs.as_ref();
        assert_eq!(s, "a/b");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn as_ref_path_returns_current_path() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "asref-path").await;
        let (env, _, _) = make_env();
        let vfs = new_vfs(&log, &env).await.entry("a/b").await;
        let p: &Path = vfs.as_ref();
        assert_eq!(p, Path::new("a/b"));
    }
}
