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

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use snafu::ResultExt;

use crate::{
    context::Log,
    environment::{EnvResult, Environment, error, shell_quote, shell_quote_path},
    storage::Id,
};

/// Directory, relative to the environment root, that holds the temporary
/// files [`Vfs::output`] captures stdout into. Entries under it are excluded
/// from [`Vfs::list`] and the `find_files*` results.
const SCRATCH_DIR: &str = ".edo-vfs";

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
                &format!(
                    "stat -- {} > /dev/null 2> /dev/null",
                    shell_quote_path(&path)
                ),
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
                &format!("mkdir -p -- {}", shell_quote_path(&path)),
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

    /// Return the target of the symbolic link at `path` (analogous to
    /// [`tokio::fs::read_link`]). The target is returned verbatim, without
    /// being resolved against the link's directory.
    pub async fn read_link(&self, path: impl AsRef<Path>) -> EnvResult<PathBuf> {
        let path = self.canonicalize(path).await?;
        let path_q = shell_quote_path(&path);
        let out = self
            .output("read_link", "readlink", ["--", path_q.as_str()])
            .await?;
        let target = out.strip_suffix(b"\n").unwrap_or(&out);
        Ok(PathBuf::from(OsStr::from_bytes(target)))
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
            .execute(
                &self.log,
                &self.id,
                &self.path,
                &format!("rm -- {}", shell_quote_path(&path)),
            )
            .await?
        {
            return error::VfsSnafu { action: "rm" }.fail();
        }
        Ok(())
    }

    /// Remove a single file (or symlink, dangling or not) at `path`,
    /// succeeding if nothing exists there.
    ///
    /// Fails with an [`error::VfsSnafu`] if the `rm -f` command reports a
    /// non-zero exit status (e.g. `path` is a directory).
    pub async fn try_remove_file(&self, path: impl AsRef<Path>) -> EnvResult<()> {
        let path = self.canonicalize(path).await?;
        trace!(
            subsystem = "environment",
            component = "vfs",
            op = "rm",
            "removing file at {path:?} from environment if present"
        );
        let path_q = shell_quote_path(&path);
        self.command("rm", "rm", ["-f", "--", path_q.as_str()])
            .await
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
            .execute(
                &self.log,
                &self.id,
                &self.path,
                &format!("rm -r -- {}", shell_quote_path(&path)),
            )
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
                &format!(
                    "cp -r -- {} {}",
                    shell_quote_path(&from),
                    shell_quote_path(&to)
                ),
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
                &format!(
                    "mv -- {} {}",
                    shell_quote_path(&from),
                    shell_quote_path(&to)
                ),
            )
            .await?
        {
            return error::VfsSnafu { action: "rename" }.fail();
        }
        Ok(())
    }

    /// List the entries directly inside the directory at `path`.
    ///
    /// Returns sorted absolute paths, excluding `.` and `..`.
    pub async fn list(&self, path: impl AsRef<Path>) -> EnvResult<Vec<PathBuf>> {
        self.find("list", path, false, &["-mindepth", "1", "-maxdepth", "1"])
            .await
    }

    /// Find regular files directly inside the directory at `path` whose
    /// name matches `glob`.
    ///
    /// `glob` is a `find -name` pattern matched against the file name only,
    /// so a pattern containing `/` (e.g. `sub/*.rs`) never matches. With
    /// `follow_symlinks`, symlinks to regular files are included. Returns
    /// sorted absolute paths.
    pub async fn find_files(
        &self,
        path: impl AsRef<Path>,
        glob: &str,
        follow_symlinks: bool,
    ) -> EnvResult<Vec<PathBuf>> {
        let glob_q = shell_quote(glob);
        self.find(
            "find_files",
            path,
            follow_symlinks,
            &[
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-type",
                "f",
                "-name",
                &glob_q,
            ],
        )
        .await
    }

    /// Find regular files anywhere beneath the directory at `path` whose
    /// name matches `glob`.
    ///
    /// `glob` is a `find -name` pattern matched against the file name only,
    /// so a pattern containing `/` (e.g. `sub/*.rs`) never matches. With
    /// `follow_symlinks`, symlinked directories are descended into and
    /// symlinks to regular files are included. Returns sorted absolute paths.
    pub async fn find_files_recursive(
        &self,
        path: impl AsRef<Path>,
        glob: &str,
        follow_symlinks: bool,
    ) -> EnvResult<Vec<PathBuf>> {
        let glob_q = shell_quote(glob);
        self.find(
            "find_files_recursive",
            path,
            follow_symlinks,
            &["-type", "f", "-name", &glob_q],
        )
        .await
    }

    /// Run `find [-L] <path> <filters...> -print0` and parse the results,
    /// dropping anything under [`SCRATCH_DIR`].
    async fn find(
        &self,
        action: &str,
        path: impl AsRef<Path>,
        follow_symlinks: bool,
        filters: &[&str],
    ) -> EnvResult<Vec<PathBuf>> {
        let dir = self.canonicalize(path).await?;
        let dir_q = shell_quote_path(&dir);
        let mut args: Vec<&str> = Vec::new();
        if follow_symlinks {
            args.push("-L");
        }
        args.push(&dir_q);
        args.extend_from_slice(filters);
        args.push("-print0");
        let out = self.output(action, "find", &args).await?;
        let scratch = self.scratch_dir().await?;
        Ok(parse_find_output(&out, &scratch))
    }

    /// Return the size in bytes of the file at `path`.
    ///
    /// Symlinks are followed, so this reports the target's size like
    /// [`tokio::fs::metadata`]. Fails if `path` is a directory.
    pub async fn size(&self, path: impl AsRef<Path>) -> EnvResult<u64> {
        let path = self.canonicalize(path).await?;
        let quoted = shell_quote_path(&path);
        // `wc -c < file` is portable across GNU, BSD and busybox; BSD pads
        // the count with leading spaces.
        let out = self
            .output("size", "wc", ["-c", "<", quoted.as_str()])
            .await?;
        let text = String::from_utf8_lossy(&out);
        let trimmed = text.trim();
        trimmed
            .parse::<u64>()
            .context(error::SizeOpSnafu { path, trimmed })
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
    /// [`Vfs`]'s current working path and capture its stdout.
    ///
    /// `action` is a short label used in [`error::VfsSnafu`] if the command
    /// reports a non-zero exit status. Arguments are joined with single
    /// spaces — callers needing shell-quoting must quote themselves.
    ///
    /// Stdout is captured in a temporary file under `.edo-vfs/` at the
    /// environment root, which is removed whether or not the command
    /// succeeds.
    pub async fn output<S, A, I>(&self, action: &str, program: S, args: A) -> EnvResult<Vec<u8>>
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
            program.as_ref()
        );
        let scratch = self.scratch_dir().await?;
        let filepath = scratch.join(names::Generator::default().next().unwrap());
        let scratch_q = shell_quote_path(&scratch);
        let file_q = shell_quote_path(&filepath);
        let ran = self
            .env
            .execute(
                &self.log,
                &self.id,
                &self.path,
                &format!(
                    "mkdir -p -- {scratch_q} && {{ {} {}; }} > {file_q}",
                    program.as_ref(),
                    args.into_iter()
                        .map(|x| x.as_ref().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            )
            .await;
        let result = match ran {
            Ok(true) => self.env.read_bytes(&filepath).await,
            Ok(false) => error::VfsSnafu { action }.fail(),
            Err(e) => Err(e),
        };
        // `rmdir` only succeeds once no other capture is in flight.
        let cleaned = self
            .env
            .execute(
                &self.log,
                &self.id,
                &self.path,
                &format!("rm -f -- {file_q}; rmdir -- {scratch_q} 2> /dev/null; true"),
            )
            .await;
        let buffer = result?;
        if !cleaned? {
            return error::VfsSnafu {
                action: "output-cleanup",
            }
            .fail();
        }
        Ok(buffer)
    }

    /// Absolute path of [`SCRATCH_DIR`] inside the environment.
    async fn scratch_dir(&self) -> EnvResult<PathBuf> {
        self.env.expand(Path::new(SCRATCH_DIR)).await
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

/// Parse the stdout of a `find ... -print0` call: split on NUL, drop empty
/// entries and anything at or under `exclude`, and sort.
fn parse_find_output(bytes: &[u8], exclude: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = bytes
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| PathBuf::from(OsStr::from_bytes(entry)))
        .filter(|path| !path.starts_with(exclude))
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    //! Exercises [`Vfs`] against the host shell: the mocked environment runs
    //! each command with `sh -c` inside a temporary root directory.
    use super::*;
    use crate::context::logmgr::test_support::shared_log_manager;
    use crate::environment::{EnvironmentError, MockEnvironmentImpl};
    use std::process::Command;
    use tempfile::TempDir;

    fn shell_env(root: &Path) -> Environment {
        let mut m = MockEnvironmentImpl::new();
        let base = root.to_path_buf();
        m.expect_expand().returning(move |p| {
            Ok(if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            })
        });
        let base = root.to_path_buf();
        m.expect_execute().returning(move |_, _, path, cmd| {
            let status = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(base.join(path))
                .status()
                .map_err(|source| EnvironmentError::Io { source })?;
            Ok(status.success())
        });
        m.expect_read_bytes()
            .returning(|p| std::fs::read(p).map_err(|source| EnvironmentError::Io { source }));
        Environment::new(m)
    }

    async fn vfs(dir: &TempDir) -> Vfs {
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let log = Log::new(&shared_log_manager().await, dir.path().join("vfs.log")).unwrap();
        let id = Id::builder()
            .name("vfs-test".to_string())
            .digest("0".to_string())
            .build();
        Vfs::new(&id, &shell_env(&root), &log).await.unwrap()
    }

    #[tokio::test]
    async fn list_excludes_capture_file() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        std::fs::write(vfs.path().join("a"), b"").unwrap();
        std::fs::create_dir(vfs.path().join("sub")).unwrap();
        let listed = vfs.list(".").await.unwrap();
        assert_eq!(listed, [vfs.path().join("a"), vfs.path().join("sub")]);
        assert!(!vfs.path().join(SCRATCH_DIR).exists());
    }

    #[tokio::test]
    async fn find_files_excludes_capture_file() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        std::fs::write(vfs.path().join("a.rs"), b"").unwrap();
        std::fs::create_dir(vfs.path().join("sub")).unwrap();
        std::fs::write(vfs.path().join("sub/b.rs"), b"").unwrap();
        assert_eq!(
            vfs.find_files(".", "*", false).await.unwrap(),
            [vfs.path().join("a.rs")]
        );
        assert_eq!(
            vfs.find_files_recursive(".", "*", false).await.unwrap(),
            [vfs.path().join("a.rs"), vfs.path().join("sub/b.rs")]
        );
    }

    #[tokio::test]
    async fn find_handles_unusual_file_names() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        let newline = vfs.path().join("two\nlines");
        let non_utf8 = vfs.path().join(OsStr::from_bytes(b"bad\xff"));
        let quoted = vfs.path().join("it's $(x)");
        for p in [&newline, &non_utf8, &quoted] {
            std::fs::write(p, b"").unwrap();
        }
        let mut expected = vec![newline, non_utf8, quoted];
        expected.sort();
        assert_eq!(vfs.list(".").await.unwrap(), expected);
    }

    #[tokio::test]
    async fn find_recursive_follows_symlinks_on_request() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("f.txt"), b"").unwrap();
        std::os::unix::fs::symlink(&target, vfs.path().join("link")).unwrap();
        assert!(
            vfs.find_files_recursive(".", "*.txt", false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            vfs.find_files_recursive(".", "*.txt", true).await.unwrap(),
            [vfs.path().join("link/f.txt")]
        );
    }

    #[tokio::test]
    async fn failed_output_cleans_up_capture_file() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        assert!(vfs.size("missing").await.is_err());
        assert!(vfs.read_link("missing").await.is_err());
        assert!(!vfs.path().join(SCRATCH_DIR).exists());
        assert!(vfs.list(".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn size_reports_bytes_and_follows_symlinks() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        std::fs::write(vfs.path().join("f"), b"hello").unwrap();
        std::os::unix::fs::symlink("f", vfs.path().join("l")).unwrap();
        assert_eq!(vfs.size("f").await.unwrap(), 5);
        assert_eq!(vfs.size("l").await.unwrap(), 5);
        std::fs::create_dir(vfs.path().join("d")).unwrap();
        assert!(vfs.size("d").await.is_err());
    }

    #[tokio::test]
    async fn read_link_preserves_whitespace() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        std::os::unix::fs::symlink(" target \n", vfs.path().join("l")).unwrap();
        assert_eq!(
            vfs.read_link("l").await.unwrap(),
            PathBuf::from(" target \n")
        );
    }

    #[tokio::test]
    async fn try_remove_file_handles_missing_and_dangling() {
        let dir = TempDir::new().unwrap();
        let vfs = vfs(&dir).await;
        vfs.try_remove_file("missing").await.unwrap();
        let dangling = vfs.path().join("dangling");
        std::os::unix::fs::symlink("nowhere", &dangling).unwrap();
        vfs.try_remove_file("dangling").await.unwrap();
        assert!(std::fs::symlink_metadata(&dangling).is_err());
        let odd = vfs.path().join("$(touch pwned)");
        std::fs::write(&odd, b"").unwrap();
        vfs.try_remove_file("$(touch pwned)").await.unwrap();
        assert!(!odd.exists());
        assert!(!vfs.path().join("pwned").exists());
    }
}
