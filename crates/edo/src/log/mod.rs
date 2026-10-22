//! Canonical enums for the `ui_*` / `header!` / `summary!` diagnostic
//! macros.
//!
//! The `#[macro_export]` macros in this module splat structured fields into tracing
//! events. [`TaskStatus`] provides a small, typed vocabulary for the
//! `status` field so call sites don't stringly-type their status names;
//! its `Display` impl emits stable kebab-case strings that show up
//! verbatim in the JSONL log and the rendered console line.

mod error;
mod macros;

use std::{fmt, str::FromStr, time::Duration};

pub use error::ParseTaskStatusError;

/// Lifecycle status of a scheduler task.
///
/// Emitted as the `status` structured field by `ui_start_task!` /
/// `ui_update_task!`. Values are `Display`-formatted as kebab-case
/// strings so grep works against the JSONL log.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
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

    /// Whether this status ends the task's lifecycle.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Wait | Self::Running => false,
            Self::Cached | Self::Success | Self::Failed | Self::Canceled => true,
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = ParseTaskStatusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "waiting" => Self::Wait,
            "running" => Self::Running,
            "cached" => Self::Cached,
            "success" => Self::Success,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            other => return Err(ParseTaskStatusError::new(other)),
        })
    }
}

/// Compact human duration: `Nms`, `N.Ns`, `NmNs`, `NhNm`.
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.subsec_millis());
    }
    if secs < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let (mins, rem) = (secs / 60, secs % 60);
    if mins < 60 {
        return format!("{mins}m{rem}s");
    }
    format!("{}h{}m", mins / 60, mins % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(TaskStatus, &str); 6] = [
        (TaskStatus::Wait, "waiting"),
        (TaskStatus::Running, "running"),
        (TaskStatus::Cached, "cached"),
        (TaskStatus::Success, "success"),
        (TaskStatus::Failed, "failed"),
        (TaskStatus::Canceled, "canceled"),
    ];

    #[test]
    fn test_task_status_display_is_stable_kebab_case() {
        for (status, expected) in ALL {
            assert_eq!(status.as_str(), expected);
            assert_eq!(status.to_string(), expected);
        }
    }

    #[test]
    fn test_task_status_from_str_round_trips_display() {
        for (status, _) in ALL {
            assert_eq!(status.to_string().parse::<TaskStatus>().unwrap(), status);
        }
    }

    #[test]
    fn test_task_status_from_str_unknown_value_errors() {
        let err = "cancelled".parse::<TaskStatus>().unwrap_err();
        assert_eq!(err.to_string(), "unknown task status \"cancelled\"");
    }

    #[test]
    fn test_task_status_is_terminal_only_for_end_states() {
        let terminal: Vec<TaskStatus> = ALL
            .iter()
            .map(|(status, _)| *status)
            .filter(TaskStatus::is_terminal)
            .collect();
        assert_eq!(
            terminal,
            [
                TaskStatus::Cached,
                TaskStatus::Success,
                TaskStatus::Failed,
                TaskStatus::Canceled
            ]
        );
    }

    #[test]
    fn test_format_duration_unit_boundaries() {
        assert_eq!(format_duration(Duration::ZERO), "0ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(1_000)), "1.0s");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(format_duration(Duration::from_secs(3_599)), "59m59s");
        assert_eq!(format_duration(Duration::from_secs(3_660)), "1h1m");
    }
}
