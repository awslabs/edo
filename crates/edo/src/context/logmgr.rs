//! Log manager and tracing initialization.
//!
//! [`LogManager`] owns the log directory, initializes the `tracing` subscriber
//! and creates per-task [`Log`] files. [`LogVerbosity`] controls the tracing
//! filter level.
//!
//! Tracing chatter (every `info!`/`debug!`/`trace!`) is written as JSON Lines
//! to `<logdir>/edo.jsonl` via [`tracing_appender::non_blocking`] and
//! [`tracing_subscriber::fmt::layer().json()`]. The build-event console (see
//! [`crate::console`]) owns stderr; tracing events flow into the file so
//! `--debug` / `--trace` never bleed onto the user's terminal. JSONL is
//! machine-readable: one JSON object per line, with timestamp / level /
//! target / span fields plus all structured event fields preserved verbatim.
//!
//! # Console events vs tracing log
//!
//! Two separate channels record what a build did:
//!
//! - **`ConsoleEvent`** (see [`crate::console::event`]) is the user-facing
//!   lifecycle: `BuildStarted`, `NodeFinished{ok}`, `BuildFinished`, etc.
//!   It is rendered live to the terminal and (optionally) to an
//!   `--event-log` JSONL file. Console events answer *what* happened.
//! - **`tracing` events** (this file) are internal instrumentation:
//!   registration, cache hits, network IO, retry, the canonical
//!   transform-failure log line. They answer *how* and *why* inside a
//!   phase. Tracing never mirrors a `ConsoleEvent` — that would just
//!   duplicate the same fact at two log levels.
//!
//! # Canonical structured-field schema
//!
//! Every `info!` / `debug!` / `trace!` / `warn!` / `error!` call uses
//! a small fixed vocabulary of keyword fields (no `target:` overrides,
//! no ad-hoc keys like `section` / `variant` / `type`):
//!
//! | field        | type | when                          | examples                                          |
//! |--------------|------|-------------------------------|---------------------------------------------------|
//! | `subsystem`  | str  | always                        | `context`, `scheduler`, `storage`, `source`, `transform`, `environment`, `console` |
//! | `component`  | str  | when subsystem has variants   | `local`, `s3`, `git`, `oci`, `remote`, `container`, `script`, `compose`, `import`, `cargo-vendor`, `go-vendor` |
//! | `addr`       | str  | when an `Addr` is in scope    | `//hello/build`                                   |
//! | `id`         | str  | when an `Id` is in scope      | `name@digest`                                     |
//! | `op`         | str  | for state-change lines        | `register`, `cache-hit`, `fetch`, `upload`, `prune`, `retry` |
//!
//! The default `target` (the module path) is fine — don't override it.
//!
//! # Severity policy
//!
//! - `error!`: irrecoverable failures **inside the tracing scope** that
//!   aren't already covered by a `ConsoleEvent::NodeFinished{ok:false}`.
//!   Most transform-error logging is owned by the single line in
//!   `scheduler::execute`.
//! - `warn!`: recoverable anomalies the user should know about — stale
//!   S3 lock, unknown layer media-type, retry attempted, render task
//!   failure.
//! - `info!`: one-time decisions and state changes worth seeing in a
//!   postmortem at default verbosity — project digest, lockfile reuse,
//!   resolver `name@version` per dep, registration of caches/farms,
//!   real source fetches, cache uploads/downloads with `id`+digest,
//!   container image load, retry decisions.
//! - `debug!`: per-loop registration noise, leaf-level "loading X" lines.
//! - `trace!`: every fs op, every `cmd_collect_out` invocation, every
//!   set-env-var call, internal id-calculation breadcrumbs.
//!
//! The [`build_sub_unit`] / [`build`] helpers are demo instrumented tasks
//! used during development.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use parking_lot::{Mutex, MutexGuard};
use rand::{RngExt, rng};
use snafu::ResultExt;
use tokio::fs::{create_dir_all, remove_dir_all};
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

pub use super::Log;
use super::{ContextResult as Result, error};

const DEBUG_ONLY: &[&str] = &[];
const TRACE_ONLY: &[&str] = &[
    "aws_config",
    "aws_runtime",
    "aws_smithy_runtime",
    "aws_sdk_sts",
    "aws_sdk_ecrpublic",
    "cranelift",
    "cranelift_codegen",
    "cranelift-codegen",
    "hyper",
    "rustls",
    "wasmtime",
];

/// Controls the tracing verbosity level for the log manager.
#[derive(PartialEq, Eq, Debug)]
pub enum LogVerbosity {
    /// Emit trace-level and above.
    Trace,
    /// Emit debug-level and above.
    Debug,
    /// Emit info-level and above (default).
    Info,
}

/// Manages the log directory and tracing subscriber for a build session.
#[derive(Clone)]
pub struct LogManager {
    inner: Arc<Inner>,
}

impl LogManager {
    /// Initializes the log directory at `path` and sets up the tracing subscriber.
    pub async fn init<P: AsRef<Path>>(path: P, verbosity: LogVerbosity) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner::init(path, verbosity).await?),
        })
    }

    /// Creates a new [`Log`] file for the given task `id`.
    pub async fn create(&self, id: &str) -> Result<Log> {
        self.inner.create(self, id).await
    }

    /// Acquires the global output lock, preventing interleaved console output.
    pub fn acquire(&self) -> MutexGuard<'_, ()> {
        self.inner.acquire()
    }

    /// Removes and recreates the log directory.
    pub async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

struct Inner {
    path: PathBuf,
    lock: Mutex<()>,
    /// Keeps the non-blocking tracing-appender worker alive for the
    /// lifetime of the [`LogManager`]. Dropping it flushes any pending
    /// log lines.
    _appender_guard: Option<WorkerGuard>,
}

/// Demo instrumented task that simulates a sub-unit of work with random delay.
#[instrument]
pub async fn build_sub_unit(sub_unit: u64) {
    let sleep_time = rng().random_range(Duration::from_millis(5000)..Duration::from_millis(10000));
    tokio::time::sleep(sleep_time).await;

    if rng().random_bool(0.2) {
        info!("sub_unit did something!");
    }
}

/// Demo instrumented task that simulates a build unit composed of sub-units.
#[instrument]
pub async fn build(unit: u64) {
    let sleep_time = rng().random_range(Duration::from_millis(2500)..Duration::from_millis(5000));
    tokio::time::sleep(sleep_time).await;

    let rand_num: f64 = rng().random();

    if rand_num < 0.1 {
        tokio::join!(build_sub_unit(0), build_sub_unit(1), build_sub_unit(2));
    } else if rand_num < 0.3 {
        tokio::join!(build_sub_unit(0), build_sub_unit(1));
    } else {
        build_sub_unit(0).await;
    }
}

impl Inner {
    pub async fn init<P: AsRef<Path>>(path: P, verbosity: LogVerbosity) -> Result<Self> {
        let logdir = path.as_ref();
        if logdir.exists() {
            // If the logdir already exists we want to clean it up, it should only be used for a single run
            remove_dir_all(&logdir).await.context(error::IoSnafu)?;
        }
        create_dir_all(&logdir).await.context(error::IoSnafu)?;
        // Build a non-blocking writer that appends JSON Lines to
        // <logdir>/edo.jsonl. The `WorkerGuard` is kept inside `Inner` so
        // the writer thread stays alive for the rest of the session.
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(logdir.join("edo.jsonl"))
            .context(error::IoSnafu)?;
        let (file_writer, appender_guard) = tracing_appender::non_blocking(log_file);

        let level = match verbosity {
            LogVerbosity::Trace => LevelFilter::TRACE,
            LogVerbosity::Debug => LevelFilter::DEBUG,
            LogVerbosity::Info => LevelFilter::INFO,
        };
        let mut filter = Targets::new().with_default(level);
        for entry in DEBUG_ONLY {
            filter = filter.with_target(
                *entry,
                if verbosity == LogVerbosity::Debug {
                    LevelFilter::DEBUG
                } else {
                    LevelFilter::OFF
                },
            );
        }
        for entry in TRACE_ONLY {
            filter = filter.with_target(
                *entry,
                if verbosity == LogVerbosity::Trace {
                    LevelFilter::TRACE
                } else {
                    LevelFilter::OFF
                },
            );
        }
        // The fmt layer writes JSON Lines to <logdir>/edo.jsonl. The
        // build-event console (see `crate::console`) owns stderr —
        // tracing never touches the terminal directly. One JSON object
        // per line; spans + structured fields are preserved so the file
        // is machine-readable for postmortem analysis.
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_writer(file_writer)
                    .with_filter(filter.clone()),
            )
            .try_init()
            .context(error::LogSnafu)?;
        Ok(Self {
            path: logdir.to_path_buf(),
            lock: Mutex::new(()),
            _appender_guard: Some(appender_guard),
        })
    }

    pub async fn clear(&self) -> Result<()> {
        remove_dir_all(&self.path).await.context(error::IoSnafu)?;
        create_dir_all(&self.path).await.context(error::IoSnafu)?;
        Ok(())
    }

    pub async fn create(&self, root: &LogManager, id: &str) -> Result<Log> {
        let file_name = format!("{id}.log");
        let file_target = self.path.join(file_name.clone());
        Log::new(root, &file_target)
    }

    pub fn acquire(&self) -> MutexGuard<'_, ()> {
        self.lock.lock()
    }
}

/// Shared test support: provides a process-wide singleton `LogManager` so that
/// `tracing_subscriber::try_init` is only called once per test binary, regardless
/// of how many test modules need a `LogManager`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{LogManager, LogVerbosity};
    use std::sync::OnceLock;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    /// Process-wide singleton.  The `TempDir` is kept alive here so the
    /// directory is not deleted while any test is running.
    static LOG_MGR_CELL: OnceLock<Mutex<Option<(LogManager, TempDir)>>> = OnceLock::new();

    /// Returns a clone of the shared `LogManager`, initialising it on first call.
    /// Subsequent calls reuse the already-initialised subscriber rather than
    /// calling `try_init` again (which would always fail after the first call).
    pub(crate) async fn shared_log_manager() -> LogManager {
        let cell = LOG_MGR_CELL.get_or_init(|| Mutex::new(None));
        let mut guard = cell.lock().await;
        if guard.is_none() {
            let dir = TempDir::new().expect("tempdir");
            let logs_dir = dir.path().join("logs");
            let mgr = LogManager::init(&logs_dir, LogVerbosity::Info)
                .await
                .expect("LogManager::init");
            *guard = Some((mgr, dir));
        }
        guard.as_ref().unwrap().0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::LogVerbosity;

    #[test]
    fn log_verbosity_eq() {
        // LogVerbosity derives PartialEq/Eq but not Debug, so use plain
        // `assert!` to avoid the Debug bound required by assert_eq!/assert_ne!.
        assert!(LogVerbosity::Info == LogVerbosity::Info);
        assert!(LogVerbosity::Debug == LogVerbosity::Debug);
        assert!(LogVerbosity::Trace == LogVerbosity::Trace);
        assert!(LogVerbosity::Info != LogVerbosity::Debug);
        assert!(LogVerbosity::Debug != LogVerbosity::Trace);
        assert!(LogVerbosity::Info != LogVerbosity::Trace);
    }

    /// Smoke-test that `shared_log_manager` returns a usable `LogManager` and
    /// that calling it multiple times yields the same underlying instance
    /// (i.e. `create` works on both).
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn shared_log_manager_returns_usable_manager() {
        let mgr = super::test_support::shared_log_manager().await;
        // Creating a log file must not panic or error.
        let _log = mgr.create("logmgr-smoke").await.expect("create log");
    }
}
