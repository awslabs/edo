use super::LogManager;
use super::{error, ContextResult as Result};
use parking_lot::Mutex;
use snafu::ResultExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::fd::IntoRawFd,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct Log {
    manager: LogManager,
    inner: Arc<Mutex<Inner>>,
}

pub struct Inner {
    path: PathBuf,
    file: File,
}

impl Log {
    pub fn new<P: AsRef<Path>>(manager: &LogManager, path: P) -> Result<Self> {
        Ok(Self {
            manager: manager.clone(),
            inner: Arc::new(Mutex::new(Inner {
                path: path.as_ref().to_path_buf(),
                file: OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.as_ref())
                    .context(error::IoSnafu)?,
            })),
        })
    }

    pub fn root(&self) -> &LogManager {
        &self.manager
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().path.clone()
    }

    pub fn log_name(&self) -> String {
        self.inner
            .lock()
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    pub fn set_subject(&self, subject: &str) {
        let _ = self
            .inner
            .lock()
            .file
            .write_fmt(format_args!("\n------ {subject} ------\n"));
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
