//! Concrete sinks for [`ConsoleEvent`]s: JSONL writer, simple stderr renderer.
//!
//! The canvas (ratatui) sink lives in `render.rs` and is added in phase 3.
//!
//! ## Sink contract
//!
//! - Every sink is `Send + Sync` and holds its own internal locking.
//! - Sinks must be infallible. IO errors get logged via `tracing::warn`
//!   and silently dropped — a build must not fail because the
//!   console-mode wrote to a closed pipe.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::Sink;
use super::event::{ConsoleEvent, Phase, Severity};

/// User-selectable console mode.
///
/// Mirrors Buck2's `--console` vocabulary. Maps from the CLI flag in
/// `crates/cli/src/cmd/mod.rs`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConsoleMode {
    /// Pick `Full` when stderr is a TTY, `Simple` otherwise.
    #[default]
    Auto,
    /// Inline canvas (phase 3 — falls back to `Simple` for now).
    Full,
    /// One line per event to stderr. CI-friendly.
    Simple,
    /// No console output. Tracing fmt layer ships to stderr instead of the rolling file.
    None,
}

impl std::str::FromStr for ConsoleMode {
    type Err = String;

    /// Parse a `--console-mode` value, returning `Err(value)` on unknown
    /// strings so the CLI can produce a helpful error.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "auto" => Ok(ConsoleMode::Auto),
            "full" => Ok(ConsoleMode::Full),
            "simple" => Ok(ConsoleMode::Simple),
            "none" => Ok(ConsoleMode::None),
            other => Err(other.to_string()),
        }
    }
}

/// Sink that appends each [`ConsoleEvent`] as one JSON line.
///
/// File is opened in `truncate` mode at session start — matches the
/// existing `.edo/` scratch semantics: each run owns its events file.
pub struct JsonlSink {
    inner: Mutex<JsonlInner>,
    /// Public for tests / `--event-log` reporting.
    pub path: PathBuf,
}

struct JsonlInner {
    writer: BufWriter<std::fs::File>,
}

impl JsonlSink {
    /// Create or truncate the JSONL file at `path`.
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            inner: Mutex::new(JsonlInner {
                writer: BufWriter::new(file),
            }),
            path,
        })
    }
}

impl Sink for JsonlSink {
    fn handle(&self, event: &ConsoleEvent) {
        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    subsystem = "console",
                    op = "serialise",
                    "failed to serialise event: {e}"
                );
                return;
            }
        };
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = writeln!(guard.writer, "{line}") {
            tracing::warn!(
                subsystem = "console",
                op = "write",
                "failed to write event: {e}"
            );
            return;
        }
        // Flush on `BuildFinished` and on every `Failed` so the file is
        // useful even when the process is interrupted.
        match event {
            ConsoleEvent::BuildFinished { .. } | ConsoleEvent::NodeFinished { ok: false, .. } => {
                let _ = guard.writer.flush();
            }
            _ => {}
        }
    }
}

/// CI-friendly renderer: one human-readable line per event to stderr.
///
/// No locking on writes beyond the global stderr lock. Output is
/// deterministic enough to assert against in `assert_cmd` matchers.
#[derive(Default)]
pub struct SimpleSink;

impl SimpleSink {
    pub fn new() -> Self {
        Self
    }
}

impl Sink for SimpleSink {
    fn handle(&self, event: &ConsoleEvent) {
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        let _ = match event {
            ConsoleEvent::SessionStarted {
                edo_version,
                target,
                args,
                started_at_unix,
            } => {
                if args.is_empty() {
                    writeln!(
                        h,
                        "SESSION edo={edo_version} target={target} started={started_at_unix}"
                    )
                } else {
                    let arg_str = args
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    writeln!(
                        h,
                        "SESSION edo={edo_version} target={target} started={started_at_unix} args=[{arg_str}]"
                    )
                }
            }
            ConsoleEvent::ProjectLoaded {
                root,
                transforms,
                sources,
                vendors,
                farms,
                caches,
                locked,
            } => {
                writeln!(
                    h,
                    "PROJECT root={root} transforms={transforms} sources={sources} vendors={vendors} farms={farms} caches={caches} locked={locked}"
                )
            }
            ConsoleEvent::EnvSetupStarted { total } => {
                writeln!(h, "ENV-SETUP start farms={total}")
            }
            ConsoleEvent::EnvSetupFarmStarted { addr } => {
                writeln!(h, "ENV-SETUP farm {addr}")
            }
            ConsoleEvent::EnvSetupFarmFinished {
                addr,
                ok,
                elapsed_ms,
            } => {
                if *ok {
                    writeln!(h, "ENV-SETUP done {addr} ({elapsed_ms} ms)")
                } else {
                    writeln!(h, "ENV-SETUP fail {addr} ({elapsed_ms} ms)")
                }
            }
            ConsoleEvent::EnvSetupFinished { elapsed_ms } => {
                writeln!(h, "ENV-SETUP end ({elapsed_ms} ms)")
            }
            ConsoleEvent::BuildStarted { root, total } => {
                writeln!(h, "BUILD start root={root} tasks={total}")
            }
            ConsoleEvent::NodeQueued { addr, .. } => writeln!(h, "QUEUED {addr}"),
            ConsoleEvent::NodeCacheHit { addr, .. } => writeln!(h, "CACHE-HIT {addr}"),
            ConsoleEvent::NodePhase { addr, phase } => {
                writeln!(h, "PHASE {addr} {}", phase.tag())
            }
            ConsoleEvent::NodeFinished {
                addr,
                ok,
                elapsed_ms,
            } => {
                if *ok {
                    writeln!(h, "DONE {addr} ({elapsed_ms} ms)")
                } else {
                    writeln!(h, "FAIL {addr} ({elapsed_ms} ms)")
                }
            }
            ConsoleEvent::Diagnostic {
                severity,
                target,
                message,
            } => {
                let tag = match severity {
                    Severity::Info => "INFO",
                    Severity::Warn => "WARN",
                    Severity::Error => "ERROR",
                };
                writeln!(h, "{tag} [{target}] {message}")
            }
            ConsoleEvent::BuildFinished {
                ok,
                elapsed_ms,
                failed,
            } => {
                if *ok {
                    writeln!(h, "BUILD ok ({elapsed_ms} ms)")
                } else {
                    writeln!(h, "BUILD failed ({elapsed_ms} ms, {} failed)", failed.len())
                }
            }
        };
    }
}

/// Returns true when stderr looks like an interactive TTY. Best-effort —
/// we use `IsTerminal` which is accurate on Unix and conservative on
/// Windows.
pub fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Resolve [`ConsoleMode::Auto`] → `Full` or `Simple` based on stderr
/// being a TTY. Other modes pass through.
pub fn resolve_mode(mode: ConsoleMode) -> ConsoleMode {
    match mode {
        ConsoleMode::Auto => {
            if stderr_is_tty() {
                ConsoleMode::Full
            } else {
                ConsoleMode::Simple
            }
        }
        other => other,
    }
}

// Suppress unused-import warning for `Phase` when no caller in this file
// references it directly under all feature combinations.
#[allow(dead_code)]
fn _phase_use(_p: Phase) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Addr;

    #[test]
    fn console_mode_parse_round_trip() {
        use std::str::FromStr;
        assert_eq!(ConsoleMode::from_str("auto"), Ok(ConsoleMode::Auto));
        assert_eq!(ConsoleMode::from_str("full"), Ok(ConsoleMode::Full));
        assert_eq!(ConsoleMode::from_str("simple"), Ok(ConsoleMode::Simple));
        assert_eq!(ConsoleMode::from_str("none"), Ok(ConsoleMode::None));
        assert!(ConsoleMode::from_str("bogus").is_err());
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_event() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = JsonlSink::create(&path).unwrap();
        sink.handle(&ConsoleEvent::BuildStarted {
            root: Addr::parse("//a").unwrap(),
            total: 1,
        });
        sink.handle(&ConsoleEvent::BuildFinished {
            ok: true,
            elapsed_ms: 5,
            failed: vec![],
        });
        // BuildFinished triggers a flush, so the file is fully readable.
        let s = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2, "got {s}");
        assert!(lines[0].contains("\"type\":\"build-started\""));
        assert!(lines[1].contains("\"type\":\"build-finished\""));
        assert!(lines[1].contains("\"ok\":true"));
    }
}
