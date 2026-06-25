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
            .execute(
                &self.log,
                &self.id,
                &self.path(),
                &format!("stat {path:?} > /dev/null 2> /dev/null"),
            )
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
        self.env.read_bytes(&path).await
    }

    // tokio::fs::write
    pub async fn write(&self, path: impl AsRef<Path>, buffer: &[u8]) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        self.env.write_bytes(&path, buffer).await
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
            .execute(&self.log, &self.id, &self.path, &format!("rm {path:?}"))
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
