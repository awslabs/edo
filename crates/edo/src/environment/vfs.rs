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

use snafu::ResultExt;

use crate::{
    context::Log,
    environment::{EnvResult, Environment, error, shell_quote, shell_quote_path},
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
            path: env.expand(&PathBuf::new()).await?,
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "changedir",
            "changing directory in environment to {:?}",
            path.as_ref()
        );
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "stat",
            "checking for existence of {path:?}",
        );
        self.env
            .execute(
                &self.log,
                &self.id,
                self.path(),
                &format!("stat {path:?} > /dev/null 2> /dev/null"),
            )
            .await
    }

    /// Set an environment variable in the underlying [`Environment`] and
    /// record the mutation in the log.
    pub async fn set_env(&self, key: &str, value: &str) -> EnvResult<()> {
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "set-env",
            key = key,
            "setting environment variable"
        );
        self.log.record("set-env", key)?;
        self.env.set_env(key, value).await
    }

    /// Read an environment variable from the underlying [`Environment`] and
    /// record the access in the log.
    pub async fn get_env(&self, key: &str) -> EnvResult<Option<String>> {
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "get-env",
            key = key,
            "getting environment variable"
        );
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "mkdir",
            "creating directory in environment at {path:?}"
        );
        if !self
            .env
            .execute(
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

    // tokio::fs::read
    pub async fn read(&self, path: impl AsRef<Path>) -> EnvResult<Vec<u8>> {
        let path = self.canonicalize(path).await?;
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "read",
            "reading from environment at {path:?}"
        );
        self.env.read_bytes(&path).await
    }

    // tokio::fs::read_link
    pub async fn read_link(&self, path: impl AsRef<Path>) -> EnvResult<PathBuf> {
        let path = self.canonicalize(path).await?;
        let path_q = shell_quote_path(&path);
        let out = self
            .output("read_link", "readlink", &[path_q.as_str()])
            .await?;
        let text = String::from_utf8_lossy(&out);
        Ok(PathBuf::from(text.trim()))
    }

    // tokio::fs::write
    pub async fn write(&self, path: impl AsRef<Path>, buffer: &[u8]) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "write",
            "writing to {path:?} in environment"
        );
        self.env.write_bytes(&path, buffer).await
    }

    /// Remove a single file at `path` (analogous to [`tokio::fs::remove_file`]).
    ///
    /// Fails with an [`error::VfsSnafu`] if the `rm` command reports a
    /// non-zero exit status.
    // tokio::fs::remove_file
    pub async fn remove_file(&self, path: impl AsRef<Path>) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "rm",
            "removing file at {path:?} from environment"
        );
        if !self
            .env
            .execute(&self.log, &self.id, &self.path, &format!("rm {path:?}"))
            .await?
        {
            return error::VfsSnafu { action: "rm" }.fail();
        }
        Ok(())
    }

    /// Remove a single file at `path` only if it exists.
    ///
    /// Fails with an [`error::VfsSnafu`] if the `rm` command reports a
    /// non-zero exit status.
    pub async fn try_remove_file(&self, path: impl AsRef<Path>) -> EnvResult<()> {
        if self.try_exists(path.as_ref()).await? {
            self.remove_file(path.as_ref()).await?;
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "rmdir",
            "removing {path:?} recursively from environment"
        );
        if !self
            .env
            .execute(&self.log, &self.id, &self.path, &format!("rm -r {path:?}"))
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "copy",
            "copying recursively {from:?} to {to:?} in environment"
        );
        if !self
            .env
            .execute(
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "mv",
            "moving file or directory {from:?} to {to:?} in environment"
        );
        if !self
            .env
            .execute(
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

    /// List contents of a directory
    pub async fn list(&self, path: impl AsRef<Path>) -> EnvResult<Vec<PathBuf>> {
        let dir = self.canonicalize(path).await?;
        let dir_q = shell_quote_path(&dir);
        let out = self
            .output(
                "ls",
                "find",
                &[dir_q.as_str(), "-mindepth", "1", "-maxdepth", "1"],
            )
            .await?;
        Ok(parse_find_output(&out))
    }

    /// Search for files matching a pattern
    pub async fn find_files(
        &self,
        path: impl AsRef<Path>,
        glob: &str,
        follow_symlinks: bool,
    ) -> EnvResult<Vec<PathBuf>> {
        let dir = self.canonicalize(path).await?;
        let dir_q = shell_quote_path(&dir);
        let glob_q = shell_quote(glob);
        let mut args: Vec<&str> = Vec::with_capacity(11);
        if follow_symlinks {
            args.push("-L");
        }
        args.push(&dir_q);
        args.extend_from_slice(&["-mindepth", "1", "-maxdepth", "1", "-type", "f", "-name"]);
        args.push(&glob_q);
        let out = self.output("find_files", "find", &args).await?;
        Ok(parse_find_output(&out))
    }

    /// Search for files matching a pattern recursively
    pub async fn find_files_recursive(
        &self,
        path: impl AsRef<Path>,
        glob: &str,
    ) -> EnvResult<Vec<PathBuf>> {
        let dir = self.canonicalize(path).await?;
        let dir_q = shell_quote_path(&dir);
        let glob_q = shell_quote(glob);
        let out = self
            .output(
                "find_files_recursive",
                "find",
                &[dir_q.as_str(), "-type", "f", "-name", glob_q.as_str()],
            )
            .await?;
        Ok(parse_find_output(&out))
    }

    /// Return the size in bytes of `path`.
    pub async fn size(&self, path: impl AsRef<Path>) -> EnvResult<u64> {
        let path = self.canonicalize(path).await?;
        let quoted = shell_quote_path(&path);
        let out = self
            .output("stat_size", "stat", &["-c", "%s", quoted.as_str()])
            .await?;
        let text = String::from_utf8_lossy(&out);
        let trimmed = text.trim();
        trimmed
            .parse::<u64>()
            .context(error::SizeOpSnafu { trimmed })
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
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "command",
            action = action,
            "executing command in environment: {}",
            program.as_ref(),
        );
        if !self
            .env
            .execute(
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

    /// Run an arbitrary `program` with `args` in the environment at the
    /// [`Vfs`]'s current working path and capture its output
    ///
    /// `action` is a short label used in [`error::VfsSnafu`] if the command
    /// reports a non-zero exit status. Arguments are joined with single
    /// spaces — callers needing shell-quoting must quote themselves.
    pub async fn output<S, A, I>(&self, action: &str, program: S, args: A) -> EnvResult<Vec<u8>>
    where
        S: AsRef<str>,
        A: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        // We want to create a temporary file
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "command",
            action = action,
            "executing command in environment: {}",
            program.as_ref()
        );
        let filename = names::Generator::default().next().unwrap();
        let filepath = self.canonicalize(filename).await?;
        if !self
            .env
            .execute(
                &self.log,
                &self.id,
                &self.path,
                &format!(
                    "{} {} > {filepath:?}",
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
        let buffer = self.env.read_bytes(&filepath).await?;
        self.remove_file(&filepath).await?;
        Ok(buffer)
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

/// Parse the stdout of a `find ...` call: split on newlines, drop
/// empty lines, sort, and materialize as `PathBuf`s.
fn parse_find_output(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<PathBuf> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    lines.sort();
    lines
}
