//! Canonical enums for the `ui_*` / `header!` / `summary!` diagnostic
//! macros.
//!
//! The macros in [`macros`] splat structured fields into tracing
//! events. [`TaskStatus`] and [`Severity`] provide a small, typed
//! vocabulary for those fields so call sites don't stringly-type their
//! status names; the `Display` impls emit stable kebab-case strings
//! that show up verbatim in the JSONL log and the rendered console
//! line.

mod macros;

use std::fmt;

/// Lifecycle status of a scheduler task.
///
/// Emitted as the `status` structured field by `ui_start_task!` /
/// `ui_update_task!`. Values are `Display`-formatted as kebab-case
/// strings so grep works against the JSONL log.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    /// Node is queued but not yet running (dependency block or backpressure).
    Wait,
    /// Node has started work and is executing.
    Running,
    /// Node was satisfied by a cache hit; no work performed.
    Cached,
    /// Node completed successfully.
    Success,
    /// Node returned an error.
    Failed,
    /// Node was aborted due to cancellation.
    Canceled,
}

impl TaskStatus {
    /// Stable kebab-case identifier for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wait => "waiting",
            Self::Running => "running",
            Self::Cached => "cached",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity level of a diagnostic emitted by `ui_info!` / `ui_warn!` /
/// `ui_error!` / `ui_fatal!` and friends.
///
/// The macros already dispatch to the matching `tracing::*!` at
/// expansion time, so this enum exists only for the small number of
/// call sites that carry a runtime-chosen severity (currently none —
/// preserved for future use and to keep the pre-tui macro API stable).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Severity {
    /// Trace level — every fs op, every syscall wrapper.
    Trace,
    /// Debug level — per-loop registration noise.
    Debug,
    /// Info level — one-time state changes worth seeing at default verbosity.
    Info,
    /// Warn level — recoverable anomalies the user should know about.
    Warn,
    /// Error level — irrecoverable failures inside the tracing scope.
    Error,
    /// Fatal — like Error but semantically terminal.
    Fatal,
}

impl Severity {
    /// Stable kebab-case identifier for this severity.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
