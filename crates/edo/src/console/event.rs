//! Typed events the scheduler / transforms publish to the [`Console`].
//!
//! ## Why typed events
//!
//! Tracing fields are stringly-typed and span-scoped: useful for
//! diagnostics but a poor fit for "what is happening right now" data
//! that consumers (canvas renderer, JSONL log, future BEP exporters)
//! need in structured form.
//!
//! [`ConsoleEvent`] is the single source of truth for build progress.
//! Every variant is a finite, named transition of the build state
//! machine.
//!
//! Events are `Clone + Serialize` so a single emit can fan out to many
//! sinks (in-memory state aggregator, file writer, optional remote
//! exporter).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::context::Addr;
use crate::storage::Id;

/// Lifecycle phase of an in-flight transform.
///
/// One-to-one with the `Log::set_subject` calls in
/// `scheduler/graph.rs::run_transform_lifecycle` — this is the
/// structured projection of the same information.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// `prepare` — pulling sources / artifacts.
    Fetch,
    /// Sources fetched; node is parked waiting for the run-pool to pick
    /// it up. Without this state, post-fetch nodes display "FETCH"
    /// indefinitely until a transform worker dispatches them.
    Wait,
    /// `create-environment` — materializing the per-transform env.
    CreateEnv,
    /// `setup-environment` — populating the env from storage.
    Setup,
    /// `spinup environment` — boot.
    SpinUp,
    /// `staging` — moving inputs into the env.
    Stage,
    /// `execution` — the user-visible work.
    Execute,
    /// `spindown environment` — teardown.
    SpinDown,
    /// `clean environment` — best-effort cleanup.
    Clean,
}

impl Phase {
    /// Short uppercase tag suitable for terminal display.
    pub fn tag(self) -> &'static str {
        match self {
            Phase::Fetch => "FETCH",
            Phase::Wait => "WAIT",
            Phase::CreateEnv => "CREATE",
            Phase::Setup => "SETUP",
            Phase::SpinUp => "UP",
            Phase::Stage => "STAGE",
            Phase::Execute => "EXEC",
            Phase::SpinDown => "DOWN",
            Phase::Clean => "CLEAN",
        }
    }

    /// Sort priority for the active-task table: higher values render
    /// first so the visible rows are the most-progressed work
    /// (`Execute` > `Stage` > … > `Wait` > `Fetch`).
    pub fn priority(self) -> u8 {
        match self {
            Phase::Execute => 8,
            Phase::Stage => 7,
            Phase::SpinUp => 6,
            Phase::Setup => 5,
            Phase::CreateEnv => 4,
            Phase::SpinDown => 3,
            Phase::Clean => 2,
            Phase::Wait => 1,
            Phase::Fetch => 0,
        }
    }
}

/// Severity for [`ConsoleEvent::Diagnostic`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

/// Build-event emitted to every registered [`Sink`](super::Sink).
///
/// Variants carry only owned data so an event can be cloned and stashed
/// across thread boundaries without lifetime entanglement. Time is
/// expressed in milliseconds for unambiguous JSON serialisation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ConsoleEvent {
    /// Process-level header. Emitted once, before any project loading,
    /// so JSONL consumers and the canvas user both have provenance for
    /// the run: which `edo` binary, which target, which CLI args, and
    /// when it started.
    SessionStarted {
        /// Cargo-provided version (e.g. `0.1.0`) of the running binary.
        edo_version: String,
        /// Top-level address the user asked to build, formatted as a
        /// string (the `Addr` may not be parseable until the project
        /// loads, but we can still record what was requested).
        target: String,
        /// `--arg key=value` pairs the user passed on the command line,
        /// preserved as a flat list.
        args: Vec<(String, String)>,
        /// Wall-clock start time in seconds since the Unix epoch.
        started_at_unix: u64,
    },
    /// Project loader summary. Emitted once after `Project::build`
    /// finishes registering everything with the [`Context`], before
    /// the scheduler emits [`Self::BuildStarted`].
    ProjectLoaded {
        /// Absolute path to the project root (the directory walked
        /// for `edo.toml` files).
        root: String,
        /// Number of `[transform.*]` entries registered.
        transforms: usize,
        /// Number of `[source.*]` entries (post lock resolution).
        sources: usize,
        /// Number of `[vendor.*]` entries.
        vendors: usize,
        /// Number of `[environment.*]` (farm) entries.
        farms: usize,
        /// Number of `[cache.*]` (source / build / output) entries.
        caches: usize,
        /// True when the lockfile was reused; false when the resolver
        /// ran and wrote a new lock.
        locked: bool,
    },
    /// Top-of-loop notification for the pre-build farm-setup pass.
    ///
    /// Emitted by `Context::setup_environments` once before iterating
    /// over registered farms. `total` is the number of farms whose
    /// `setup()` will run (image pulls, container engine loads, etc).
    ///
    /// Sequenced **after** [`Self::ProjectLoaded`] and **before**
    /// [`Self::BuildStarted`] so consumers can show "setting up
    /// environments \u2026" between project load and the scheduler
    /// kicking off node activity.
    EnvSetupStarted { total: usize },
    /// A single farm's `setup()` is starting. Useful so the canvas can
    /// surface the in-flight farm address (a container farm's setup
    /// can take seconds to minutes — image pull + load + tag — and
    /// without this event the UI would show a blank screen between
    /// `ProjectLoaded` and `BuildStarted`).
    EnvSetupFarmStarted { addr: Addr },
    /// A single farm's `setup()` has returned. `ok=false` means setup
    /// failed; `Context::run` will propagate the error and the build
    /// won't start.
    EnvSetupFarmFinished {
        addr: Addr,
        ok: bool,
        elapsed_ms: u64,
    },
    /// Bottom-of-loop notification for the pre-build farm-setup pass.
    /// Always emitted, even if zero farms were registered, so consumers
    /// have a definite "setup phase ended" signal before
    /// [`Self::BuildStarted`].
    EnvSetupFinished { elapsed_ms: u64 },
    /// Marks the start of a top-level build for `root`. Emitted once
    /// before any node activity. `total` is the count of nodes in the
    /// reachable subgraph (post transitive reduction).
    BuildStarted { root: Addr, total: usize },
    /// A node has been added to the active subgraph. Emitted by
    /// `Graph::add` after the indegree template is computed.
    NodeQueued { addr: Addr, id: Option<Id> },
    /// A node hit the build cache and will not run.
    NodeCacheHit { addr: Addr, id: Id },
    /// A node entered a new lifecycle phase.
    NodePhase { addr: Addr, phase: Phase },
    /// A node finished. `ok=false` means the transform errored; the
    /// scheduler will short-circuit.
    NodeFinished {
        addr: Addr,
        ok: bool,
        elapsed_ms: u64,
    },
    /// User-visible diagnostic. Use sparingly — most `info!`/`debug!`
    /// stays in the rolling log.
    Diagnostic {
        severity: Severity,
        target: String,
        message: String,
    },
    /// Final summary. Emitted once before [`Console::shutdown`] returns.
    BuildFinished {
        ok: bool,
        elapsed_ms: u64,
        failed: Vec<Addr>,
    },
}

impl ConsoleEvent {
    /// Convenience: construct a [`ConsoleEvent::Diagnostic`] from a `Duration`-free signature.
    pub fn diag(severity: Severity, target: impl Into<String>, message: impl Into<String>) -> Self {
        ConsoleEvent::Diagnostic {
            severity,
            target: target.into(),
            message: message.into(),
        }
    }
}

/// Helper: convert a [`Duration`] to milliseconds capped at `u64::MAX`.
pub fn duration_ms(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_round_trips_json() {
        for p in [
            Phase::Fetch,
            Phase::CreateEnv,
            Phase::Setup,
            Phase::SpinUp,
            Phase::Stage,
            Phase::Execute,
            Phase::SpinDown,
            Phase::Clean,
        ] {
            let s = serde_json::to_string(&p).unwrap();
            let back: Phase = serde_json::from_str(&s).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn build_started_serializes_with_type_tag() {
        let ev = ConsoleEvent::BuildStarted {
            root: Addr::parse("//foo").unwrap(),
            total: 3,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"build-started\""), "got {s}");
        assert!(s.contains("\"total\":3"), "got {s}");
    }

    #[test]
    fn diagnostic_severity_lowercases() {
        let ev = ConsoleEvent::diag(Severity::Warn, "test", "hello");
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"severity\":\"warn\""), "got {s}");
    }

    #[test]
    fn duration_ms_basic() {
        assert_eq!(duration_ms(Duration::from_millis(0)), 0);
        assert_eq!(duration_ms(Duration::from_millis(123)), 123);
        assert_eq!(duration_ms(Duration::from_secs(2)), 2000);
    }
}
