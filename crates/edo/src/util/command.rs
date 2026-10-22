use crate::context::Log;
use std::ffi::OsString;
use std::io::Result;
use std::os::fd::IntoRawFd;
use std::path::Path;

use dashmap::DashMap;
use duct::IntoExecutablePath;
use std::collections::HashMap;
use std::io::Write;

/// Convert a [`DashMap`] into a standard [`HashMap`] by cloning all entries.
pub fn from_dash<K, V>(input: &DashMap<K, V>) -> HashMap<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + Clone,
    V: Clone,
{
    input
        .iter()
        .map(|x| (x.key().clone(), x.value().clone()))
        .collect()
}

/// Run a command with piped stdin, capturing stdout+stderr to the build log.
///
/// Returns `true` if the process exits successfully.
pub fn cmd<P, S, In, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    input: &mut In,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    In: std::io::Read,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let (pipe_reader, mut pipe_writer) = os_pipe::pipe()?;
    let mut expr = duct::cmd(program, args)
        .dir(path.as_ref())
        .stderr_to_stdout()
        .stdout_file(log)
        .stdin_file(pipe_reader);
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }

    let handle = expr.unchecked().start()?;
    std::io::copy(input, &mut pipe_writer)?;
    pipe_writer.flush()?;
    drop(pipe_writer);
    let output = handle.wait()?;
    Ok(output.status.success())
}

/// Run a command capturing stdout into a byte vector; stderr goes to the log.
///
/// Stdin is `/dev/null`: a build command must never compete with the
/// scheduler's interactive failure prompt for the user's terminal.
pub fn cmd_collect_out<P, S, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<Vec<u8>>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdin_null()
        .stdout_capture()
        .stderr_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.stdout)
}

/// Run a command piping stdout to a raw file descriptor; stderr goes to the log.
///
/// Stdin is `/dev/null` — see [`cmd_collect_out`].
///
/// Returns `true` if the process exits successfully.
pub fn cmd_pipeout<P, F, S, A, I>(
    path: P,
    log: &Log,
    out: F,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    F: IntoRawFd,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdin_null()
        .stdout_file(out)
        .stderr_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.status.success())
}

/// Run a command with no stdin, merging stdout+stderr to the build log.
///
/// Stdin is `/dev/null` — see [`cmd_collect_out`].
///
/// Returns `true` if the process exits successfully.
pub fn cmd_noinput<P, S, A, I>(
    path: P,
    log: &Log,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdin_null()
        .stderr_to_stdout()
        .stdout_file(log)
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;

    Ok(output.status.success())
}

/// Run a command inheriting the parent process's stdin, stdout and stderr
/// (no log redirection).
///
/// This is the only helper that hands the terminal to the child; it exists
/// for `Environment::shell`, where the user drives the child directly.
///
/// Returns `true` if the process exits successfully.
pub fn cmd_noredirect<P, S, A, I>(
    path: P,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args).dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.unchecked().run()?;
    Ok(output.status.success())
}

/// Run a command discarding both stdout and stderr.
///
/// Stdin is `/dev/null` — see [`cmd_collect_out`].
///
/// Returns `true` if the process exits successfully.
pub fn cmd_nulled<P, S, A, I>(
    path: P,
    program: S,
    args: I,
    env: &HashMap<String, String>,
) -> Result<bool>
where
    P: AsRef<Path>,
    S: IntoExecutablePath,
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let mut expr = duct::cmd(program, args)
        .stdin_null()
        .stdout_null()
        .stderr_null()
        .unchecked()
        .dir(path.as_ref());
    for (key, value) in env.iter() {
        expr = expr.env(key.clone(), value.clone());
    }
    let output = expr.run()?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    //! The build-command helpers must never hand the user's terminal to a
    //! child: a child that reads stdin competes with the scheduler's
    //! interactive failure prompt for keystrokes.

    use super::{cmd_collect_out, cmd_noinput};
    use crate::context::logmgr::test_support::shared_log_manager;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn cmd_noinput_gives_the_child_an_empty_stdin() {
        let mgr = shared_log_manager().await;
        let log = mgr.create("cmd-noinput-stdin").await.expect("log");
        let dir = TempDir::new().unwrap();

        // `wc -c` drains stdin, so it reports 0 only when stdin is null.
        let ok = cmd_noinput(dir.path(), &log, "sh", ["-c", "wc -c"], &HashMap::new())
            .expect("spawn sh");
        assert!(ok, "wc should exit successfully");

        let captured = std::fs::read_to_string(log.path()).expect("read log");
        assert_eq!(
            captured.trim(),
            "0",
            "child must see EOF on stdin, got {captured:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn cmd_collect_out_gives_the_child_an_empty_stdin() {
        let mgr = shared_log_manager().await;
        let log = mgr.create("cmd-collect-stdin").await.expect("log");
        let dir = TempDir::new().unwrap();

        let out = cmd_collect_out(dir.path(), &log, "sh", ["-c", "wc -c"], &HashMap::new())
            .expect("spawn sh");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "0");
    }
}
