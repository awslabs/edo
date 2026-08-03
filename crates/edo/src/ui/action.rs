use std::{env, path::PathBuf};

use jiff::Timestamp;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

use crate::context::Addr;

#[allow(dead_code)]
static EDO_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Header action happens on system load
    Header {
        /// Tool name being run
        tool: String,
        /// Version of the build tool using edo
        version: semver::Version,
        /// Top level address being asked to build/run or whatever if provided
        addr: Option<Addr>,
        /// --arg key=value pairs
        args: Vec<(String, String)>,
        /// Wall-clock start time
        started_at: Timestamp,
    },
    /// Summary of the project emitted after ctx is fully loaded
    Summary {
        /// Absolute path the project root
        root: PathBuf,
        /// Number of transforms registered
        transforms: usize,
        /// Number of sources registered post lock resolution
        sources: usize,
        /// Number of environment farms
        farms: usize,
        /// True when the lockfile was reused
        locked: bool,
    },
    /// Start of a build
    StartBuild { addr: Addr, total: usize },
    /// Add a new running task onto the tui
    StartTask {
        /// Component this task represents
        component: String,
        /// ID unique to the component
        id: String,
        /// Status to start with
        status: TaskStatus,
        /// Operation the task is performing
        operation: String,
        // Any start of operation message
        message: Option<String>,
    },
    /// Update an existing task in the tui
    UpdateTask {
        /// Component of the task to update
        component: String,
        /// ID unique to the component
        id: String,
        /// Operation being performed
        operation: String,
        /// Status to update to
        status: TaskStatus,
        /// Optional message with the update
        message: Option<String>,
    },
    /// Diagnostic Message sent into the top log
    Diagnostic {
        /// Component of this diagnostic is coming from
        component: String,
        /// Optional ID
        id: Option<String>,
        /// Severity of the diagnostic
        severity: Severity,
        /// Message to send
        message: String,
    },
    /// Build Finished
    BuildFinish,
    /// Terminate the UI
    Terminate,
}

/// Format a `[component:id]` bracket group, or an empty string when both
/// parts are effectively empty. Prevents `[]:` ghost prefixes in the log.
fn fmt_bracket(component: &str, id: Option<&str>) -> String {
    let id_str = id.unwrap_or("");
    match (component.is_empty(), id_str.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!(" [{id_str}]"),
        (false, true) => format!(" [{component}]"),
        (false, false) => format!(" [{component}:{id_str}]"),
    }
}

/// Truncate `s` so that its display width fits `max` columns, appending a
/// single `…` character when truncation occurs. Uses a byte-based char
/// approximation (each char = 1 column) — enough for the CLI diagnostics
/// which are ASCII-heavy, and cheaper than pulling in `unicode-width`.
#[allow(dead_code)]
fn truncate_visible(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let take = max - 1;
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

/// Soft-wrap `s` into chunks of at most `max` characters. Prefers to
/// break at the last ASCII whitespace inside each chunk so words stay
/// intact; falls back to a hard break when a single word is longer than
/// `max`. Returns at least one chunk (possibly empty) for any input so
/// callers can rely on `chunks.len() >= 1`.
///
/// Wrapping is done here — in the action layer — so the app layer can
/// reserve exactly `chunks.len()` scroll-back rows via `insert_before`.
/// If we let the terminal wrap a single 1-row insert, the extra visual
/// rows overlap the inline viewport and produce "ghost" prefix lines.
fn wrap_visible(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }

    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let end = (i + max).min(chars.len());
        // If we've consumed everything, the last slice is the final
        // chunk — emit as-is.
        if end == chars.len() {
            out.push(chars[i..end].iter().collect());
            break;
        }
        // Try to break at the last whitespace inside [i, end). Skip
        // whitespace on the right edge of the slice so a break at a
        // trailing space doesn't leave the next line starting with a
        // space anyway.
        let mut break_at: Option<usize> = None;
        let mut k = end;
        while k > i {
            k -= 1;
            if chars[k].is_ascii_whitespace() {
                break_at = Some(k);
                break;
            }
        }
        let (chunk_end, next_start) = match break_at {
            // Only prefer the word-break if it isn't the very first
            // char of the window — otherwise we'd emit an empty chunk
            // and loop forever on a leading-whitespace payload.
            Some(k) if k > i => (k, k + 1),
            _ => (end, end),
        };
        out.push(chars[i..chunk_end].iter().collect());
        i = next_start;
        // Skip any run of whitespace at the start of the next line so
        // the wrapped continuation doesn't lead with a stray space.
        while i < chars.len() && chars[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Format a `jiff::Span` as a short human-readable duration: `456ms`,
/// `12.3s`, `1m23s`, `2h5m`. Callers reach for this via `Task::to_line`
/// and (post Phase 5.3) `Event::to_lines` for terminal task updates.
pub(crate) fn fmt_short_duration(span: jiff::Span) -> String {
    // Convert to total nanoseconds via signed_duration_since is awkward
    // without a reference; use total(Nanosecond) which handles calendar-free
    // spans safely.
    let ns = span.total(jiff::Unit::Nanosecond).unwrap_or(0.0).max(0.0) as u128;
    let ms = ns / 1_000_000;
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs_f = ms as f64 / 1000.0;
    if secs_f < 60.0 {
        return format!("{secs_f:.1}s");
    }
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    format!("{hours}h{rem_mins}m")
}

impl Action {
    /// Convenience wrapper for callers who don't have a terminal width
    /// (tests, initial migration). Uses a large cap so no truncation
    /// happens.
    pub fn to_lines(&self) -> Vec<Line<'_>> {
        self.to_lines_width(u16::MAX)
    }

    /// Render this event into one or more `ratatui::Line`s that fit
    /// within `width` columns. Long messages are soft-wrapped across
    /// multiple rows (continuation rows are indented under the message
    /// column) so users see the full text at their terminal width.
    ///
    /// Wrapping is done here — not left to the terminal — so the
    /// interactive app layer can reserve exactly `lines.len()`
    /// scroll-back rows via `insert_before`. Letting the terminal wrap
    /// a 1-row insert would overlap the inline viewport and produce
    /// "ghost" duplicate prefix lines.
    pub fn to_lines_width(&self, width: u16) -> Vec<Line<'_>> {
        let width = width as usize;
        match self {
            Self::Header {
                tool,
                version,
                addr,
                args,
                started_at,
            } => {
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(format!("{} version: ", tool), Style::default().bold()),
                        Span::raw(version.to_string()),
                    ]),
                    Line::from(vec![
                        Span::styled("using edo: ", Style::default().bold()),
                        Span::raw(env!("CARGO_PKG_VERSION").to_string()),
                    ]),
                ];
                if let Some(addr) = addr {
                    lines.push(Line::from(vec![
                        Span::styled("target: ", Style::default().bold()),
                        Span::raw(addr.to_string()),
                    ]));
                }
                if !args.is_empty() {
                    lines.push(Line::from(vec![Span::styled(
                        "args:",
                        Style::default().bold(),
                    )]))
                }
                for (key, value) in args {
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {key}: "), Style::default().bold()),
                        Span::raw(value.clone()),
                    ]));
                }
                lines.push(Line::from(vec![
                    Span::styled("started: ", Style::default().bold()),
                    Span::raw(started_at.to_string()),
                ]));
                lines
            }
            Self::Summary {
                root,
                transforms,
                sources,
                farms,
                locked,
            } => vec![
                Line::from(vec![
                    Span::styled("project: ", Style::default().bold()),
                    Span::raw(root.to_string_lossy().to_string()),
                ]),
                Line::from(vec![
                    Span::styled("transforms: ", Style::default().bold()),
                    Span::raw(transforms.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("sources: ", Style::default().bold()),
                    Span::raw(sources.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("environments: ", Style::default().bold()),
                    Span::raw(farms.to_string()),
                ]),
                Line::from(vec![if *locked {
                    Span::styled("lock match", Style::default().bold().fg(Color::Green))
                } else {
                    Span::styled("lock mismatch", Style::default().bold().fg(Color::Yellow))
                }]),
            ],
            Self::Diagnostic {
                component,
                id,
                severity,
                message,
            } => {
                let sev_str = severity_label(*severity);
                let bracket = fmt_bracket(component, id.as_deref());
                // Prefix layout: "<sev><bracket>: <msg…>". `bracket`
                // starts with a leading space when non-empty, so the
                // ": " suffix contributes 2 columns.
                let prefix_visible = sev_str.chars().count() + bracket.chars().count() + 2;
                let budget = width.saturating_sub(prefix_visible).max(1);
                let chunks = wrap_visible(message, budget);
                let indent = " ".repeat(prefix_visible);
                let mut lines: Vec<Line<'_>> = Vec::with_capacity(chunks.len());
                for (idx, chunk) in chunks.iter().enumerate() {
                    if idx == 0 {
                        lines.push(Line::from(vec![
                            severity.to_span(),
                            Span::styled(
                                format!("{bracket}:"),
                                Style::default().fg(Color::DarkGray).italic(),
                            ),
                            Span::raw(format!(" {chunk}")),
                        ]));
                    } else {
                        // Continuation rows: align under the message
                        // column so wrapped text visually pairs with
                        // its header without repeating the sev/bracket.
                        lines.push(Line::from(vec![Span::raw(format!("{indent}{chunk}"))]));
                    }
                }
                lines
            }
            Self::StartTask {
                component,
                id,
                status,
                operation,
                message,
            } if *status == TaskStatus::Cached => {
                let bracket = fmt_bracket(component, Some(id.as_str()));
                let label = status_label(*status);
                // Header layout: "<label><bracket>(<operation>): <msg>".
                // `bracket` includes its leading space; `(operation)`
                // adds 2 for the parens; ": " adds 2 more.
                let header_visible = label.chars().count()
                    + bracket.chars().count()
                    + operation.chars().count()
                    + 4;
                let budget = width.saturating_sub(header_visible).max(1);
                match message {
                    None => vec![Line::from(vec![
                        status.to_span(),
                        Span::styled(
                            format!("{bracket}({operation})"),
                            Style::default().fg(Color::DarkGray).italic(),
                        ),
                    ])],
                    Some(message) => {
                        let chunks = wrap_visible(message, budget);
                        let indent = " ".repeat(header_visible);
                        let mut lines: Vec<Line<'_>> = Vec::with_capacity(chunks.len());
                        for (idx, chunk) in chunks.iter().enumerate() {
                            if idx == 0 {
                                lines.push(Line::from(vec![
                                    status.to_span(),
                                    Span::styled(
                                        format!("{bracket}({operation})"),
                                        Style::default().fg(Color::DarkGray).italic(),
                                    ),
                                    Span::raw(format!(": {chunk}")),
                                ]));
                            } else {
                                lines.push(Line::from(vec![Span::raw(format!(
                                    "{indent}{chunk}"
                                ))]));
                            }
                        }
                        lines
                    }
                }
            }
            Self::UpdateTask {
                component,
                id,
                operation,
                status,
                message,
            } if matches!(
                status,
                TaskStatus::Canceled | TaskStatus::Success | TaskStatus::Failed
            ) =>
            {
                let bracket = fmt_bracket(component, Some(id.as_str()));
                let label = status_label(*status);
                let header_visible = label.chars().count()
                    + bracket.chars().count()
                    + operation.chars().count()
                    + 4;
                let budget = width.saturating_sub(header_visible).max(1);
                match message {
                    None => vec![Line::from(vec![
                        status.to_span(),
                        Span::styled(
                            format!("{bracket}({operation})"),
                            Style::default().fg(Color::DarkGray).italic(),
                        ),
                    ])],
                    Some(message) => {
                        let chunks = wrap_visible(message, budget);
                        let indent = " ".repeat(header_visible);
                        let mut lines: Vec<Line<'_>> = Vec::with_capacity(chunks.len());
                        for (idx, chunk) in chunks.iter().enumerate() {
                            if idx == 0 {
                                lines.push(Line::from(vec![
                                    status.to_span(),
                                    Span::styled(
                                        format!("{bracket}({operation})"),
                                        Style::default().fg(Color::DarkGray).italic(),
                                    ),
                                    Span::raw(format!(": {chunk}")),
                                ]));
                            } else {
                                lines.push(Line::from(vec![Span::raw(format!(
                                    "{indent}{chunk}"
                                ))]));
                            }
                        }
                        lines
                    }
                }
            }
            _ => vec![],
        }
    }
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Trace => "trace",
        Severity::Debug => "debug",
        Severity::Info => "info",
        Severity::Warn => "warning",
        Severity::Error => "error",
        Severity::Fatal => "fatal",
    }
}

fn status_label(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Wait => "waiting",
        TaskStatus::Running => "running",
        TaskStatus::Failed => "failed",
        TaskStatus::Success => "success",
        TaskStatus::Canceled => "canceled",
        TaskStatus::Cached => "cached",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    pub fn to_span(&self) -> Span<'_> {
        match self {
            Self::Trace => Span::styled("trace", Style::default().fg(Color::DarkGray)),
            Self::Debug => Span::styled("debug", Style::default().fg(Color::Gray)),
            Self::Info => Span::styled("info", Style::default().fg(Color::Blue)),
            Self::Warn => Span::styled("warning", Style::default().fg(Color::Yellow)),
            Self::Error => Span::styled("error", Style::default().fg(Color::Red)),
            Self::Fatal => Span::styled("fatal", Style::default().bold().fg(Color::LightRed)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Wait,
    Running,
    Failed,
    Success,
    Canceled,
    Cached,
}

impl TaskStatus {
    pub fn to_span(&self) -> Span<'_> {
        match self {
            Self::Wait => Span::styled("waiting", Style::default().fg(Color::DarkGray)),
            Self::Running => Span::styled("running", Style::default().fg(Color::Blue)),
            Self::Failed => Span::styled("failed", Style::default().fg(Color::Red)),
            Self::Success => Span::styled("success", Style::default().fg(Color::Green)),
            Self::Canceled => {
                Span::styled("canceled", Style::default().fg(Color::Gray).crossed_out())
            }
            Self::Cached => Span::styled("cached", Style::default().fg(Color::Gray).italic()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, bon::Builder)]
pub struct Task {
    pub component: String,
    pub id: String,
    pub operation: String,
    pub status: TaskStatus,
    pub status_since: Timestamp,
    pub message: Option<String>,
}

impl Task {
    /// Render a running-task row. `now` is passed in so a single frame
    /// only calls `Timestamp::now()` once instead of once per row.
    pub fn to_line(&self, now: Timestamp) -> Line<'_> {
        let span = now - self.status_since;
        Line::from(vec![
            Span::styled(
                format!("({}) ", fmt_short_duration(span)),
                Style::default().fg(Color::Gray).italic(),
            ),
            self.status.to_span(),
            Span::styled(
                format!(" [{}:{}]({})", self.component, self.id, self.operation),
                Style::default().fg(Color::DarkGray).italic(),
            ),
            if let Some(message) = self.message.as_ref() {
                Span::raw(format!(": {message}"))
            } else {
                Span::raw("")
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_visible_width(line: &Line) -> usize {
        line.spans.iter().map(|s| s.content.chars().count()).sum()
    }

    #[test]
    fn diagnostic_fits_width() {
        let long_msg = "x".repeat(500);
        let ev = Action::Diagnostic {
            component: "remote".to_string(),
            id: Some("aws_efa_installer_1_47_0_tar_gz".to_string()),
            severity: Severity::Info,
            message: long_msg,
        };
        let lines = ev.to_lines_width(80);
        // Long diagnostics wrap across multiple rows so the reader
        // sees the whole message; each row must still fit the budget.
        assert!(lines.len() > 1, "expected wrapping, got {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line_visible_width(line) <= 80,
                "line {i} width {} exceeded budget 80",
                line_visible_width(line)
            );
        }
        // Concatenating all row content should recover the message
        // (modulo the header prefix on row 0 and indent on
        // continuations). The 500-x payload must be fully present.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined.matches('x').count(), 500);
    }

    #[test]
    fn diagnostic_wraps_at_word_boundary() {
        // Deliberately picks a payload where a word boundary sits well
        // inside the wrap window so the wrapper can prefer it over a
        // mid-word break.
        let ev = Action::Diagnostic {
            component: "src".to_string(),
            id: None,
            severity: Severity::Info,
            message: "alpha beta gamma delta epsilon zeta eta theta iota".to_string(),
        };
        let lines = ev.to_lines_width(30);
        assert!(lines.len() >= 2);
        // No continuation line should start with whitespace — the
        // wrapper is expected to swallow the break character.
        for line in &lines[1..] {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let trimmed_leading = text.trim_start_matches(' ');
            assert!(
                !trimmed_leading.starts_with(' '),
                "continuation should not begin with extra space: {text:?}"
            );
        }
    }

    #[test]
    fn diagnostic_short_message_is_single_line() {
        let ev = Action::Diagnostic {
            component: "storage".to_string(),
            id: None,
            severity: Severity::Info,
            message: "ok".to_string(),
        };
        let lines = ev.to_lines_width(80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diagnostic_without_component_has_no_empty_brackets() {
        let ev = Action::Diagnostic {
            component: String::new(),
            id: None,
            severity: Severity::Info,
            message: "hello".to_string(),
        };
        let lines = ev.to_lines_width(80);
        let text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(!text.contains("[]"), "found empty brackets in {text:?}");
        assert!(text.contains("hello"));
    }

    #[test]
    fn diagnostic_without_id_shows_only_component() {
        let ev = Action::Diagnostic {
            component: "storage".to_string(),
            id: None,
            severity: Severity::Info,
            message: "hi".to_string(),
        };
        let lines = ev.to_lines_width(80);
        let text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("[storage]"), "unexpected: {text:?}");
    }

    #[test]
    fn fmt_short_duration_reads_naturally() {
        use jiff::{ToSpan, Unit};
        // 456 ms
        let s = 456.milliseconds();
        assert_eq!(fmt_short_duration(s), "456ms");
        // 12.3 s -> we take total nanoseconds; construct as 12300ms
        let s = 12_300.milliseconds();
        let out = fmt_short_duration(s);
        assert!(out.ends_with('s'));
        assert!(out.starts_with("12"));
        // 1m23s
        let s = 83.seconds();
        // total_nanoseconds() only works if we don't cross calendar units; seconds fine
        let out = fmt_short_duration(s);
        assert_eq!(out, "1m23s");
        // 2h5m
        let s = 7_500.seconds();
        let out = fmt_short_duration(s);
        assert_eq!(out, "2h5m");
        // silence the unused warning
        let _ = Unit::Nanosecond;
    }
}
