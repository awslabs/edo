//! Per-task log file writer.
//!
//! [`Log`] wraps a file handle that captures output for a single build task.
//! It implements [`std::io::Write`] and [`IntoRawFd`](std::os::fd::IntoRawFd)
//! so it can be used as both a Rust writer and a raw file descriptor for
//! child processes.

use super::LogManager;
use super::{ContextResult as Result, error};
use parking_lot::Mutex;
use snafu::ResultExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A cloneable, thread-safe log file for a single build task.
#[derive(Clone)]
pub struct Log {
    manager: LogManager,
    inner: Arc<Mutex<Inner>>,
}

/// Internal state holding the file path and open file handle for a [`Log`].
pub struct Inner {
    path: PathBuf,
    subject: String,
    file: File,
}

impl Log {
    /// Creates a new log file at `path`, opening it in append mode.
    pub fn new<P: AsRef<Path>>(manager: &LogManager, path: P) -> Result<Self> {
        Ok(Self {
            manager: manager.clone(),
            inner: Arc::new(Mutex::new(Inner {
                path: path.as_ref().to_path_buf(),
                subject: "general".to_string(),
                file: OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.as_ref())
                    .context(error::IoSnafu)?,
            })),
        })
    }

    /// Returns a reference to the parent [`LogManager`].
    pub fn root(&self) -> &LogManager {
        &self.manager
    }

    /// Returns the full file path of this log.
    pub fn path(&self) -> PathBuf {
        self.inner.lock().path.clone()
    }

    /// Returns the file name component of the log path.
    pub fn log_name(&self) -> String {
        self.inner
            .lock()
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    /// Writes a section header line to the log file.
    pub fn set_subject(&self, subject: &str) {
        let _ = self
            .inner
            .lock()
            .file
            .write_fmt(format_args!("\n=== [{subject}] ===\n"));
        self.inner.lock().subject = subject.to_string();
    }

    /// Writes a dedicated action to the log file
    pub fn record(&self, action: &str, message: &str) -> Result<()> {
        let mut lock = self.inner.lock();
        let line = format!("\n> [{}]({action}): {message}\n", lock.subject.clone());
        lock.file
            .write_all(line.as_bytes())
            .context(error::IoSnafu)?;
        Ok(())
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // A log should always be receiving text data so we can operate on it as such
        let mut lock = self.inner.lock();
        lock.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().file.flush()
    }
}

impl IntoRawFd for &Log {
    fn into_raw_fd(self) -> std::os::unix::prelude::RawFd {
        let lock = self.inner.lock();
        let file = lock.file.try_clone().unwrap();
        drop(lock);
        file.into_raw_fd()
    }
}

#[macro_export]
macro_rules! record {
    ($log: ident, $action: literal, $($arg: tt)*) => {
        $log.record($action, &format!($($arg)*))?;
    };
}

#[cfg(test)]
mod tests {
    use super::Log;
    use crate::context::logmgr::test_support::shared_log_manager;
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd};
    use tempfile::TempDir;

    /// Helper: create a `Log` at `<dir>/<name>.log` using the shared manager.
    async fn make_log(dir: &TempDir, name: &str) -> Log {
        let mgr = shared_log_manager().await;
        let path = dir.path().join(format!("{name}.log"));
        Log::new(&mgr, &path).expect("Log::new")
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn log_path_returns_passed_path() {
        let dir = TempDir::new().unwrap();
        let expected = dir.path().join("x.log");
        let log = make_log(&dir, "x").await;
        assert_eq!(log.path(), expected);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn log_name_returns_file_name() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "x").await;
        assert_eq!(log.log_name(), "x.log");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn set_subject_writes_header() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "subj").await;
        log.set_subject("build");
        // Flush via the Write impl to ensure bytes are on disk.
        let mut clone = log.clone();
        clone.flush().unwrap();
        let contents = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            contents.contains("=== [build] ==="),
            "unexpected contents: {contents:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn write_appends_bytes() {
        let dir = TempDir::new().unwrap();
        let mut log = make_log(&dir, "write").await;
        log.write_all(b"hello").unwrap();
        log.write_all(b" world").unwrap();
        log.flush().unwrap();
        let contents = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            contents.ends_with("hello world"),
            "unexpected contents: {contents:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn flush_returns_ok() {
        let dir = TempDir::new().unwrap();
        let mut log = make_log(&dir, "flush").await;
        log.flush().unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn into_raw_fd_returns_non_negative() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir, "rawfd").await;
        let fd = (&log).into_raw_fd();
        assert!(fd >= 0, "expected non-negative fd, got {fd}");
        // Wrap in a File so the descriptor is properly closed.
        let _ = unsafe { std::fs::File::from_raw_fd(fd) };
    }
}
