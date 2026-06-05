//! Live aggregate of [`ConsoleEvent`]s — the state the canvas renders.
//!
//! [`BuildState`] applies events one at a time and is unit-testable in
//! isolation: feed it a deterministic event stream, assert the
//! resulting fields. The renderer reads from a snapshot.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::context::Addr;
use crate::storage::Id;

use super::event::{ConsoleEvent, Phase};

/// Per-task display state held by [`BuildState::active`].
#[derive(Clone, Debug)]
pub struct ActiveTask {
    /// Current lifecycle phase, or `None` if the task is queued but not
    /// yet entered `prepare`.
    pub phase: Option<Phase>,
    /// Wall-clock instant at which the task was first seen — used by
    /// the renderer to colour rows by elapsed time.
    pub started: Instant,
    /// Stable id, populated as soon as `Graph::fetch` reports it.
    pub id: Option<Id>,
}

/// Lifecycle classification used by the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    /// Queued, not yet running.
    Queued,
    /// Running.
    Running,
    /// Finished (success or cache hit). Removed from `active`.
    Done,
    /// Failed.
    Failed,
}

/// Aggregate of every [`ConsoleEvent`] received in the current session.
#[derive(Default)]
pub struct BuildState {
    /// Currently in-flight tasks keyed by [`Addr`] so the renderer can
    /// produce a stable, alphabetically sorted display.
    pub active: BTreeMap<Addr, ActiveTask>,
    /// Total node count from [`ConsoleEvent::BuildStarted`].
    pub total: usize,
    /// Tasks that have reached a terminal state (success or cache hit).
    pub finished: usize,
    /// Cache-hit count — the proportion that never entered execute.
    pub cache_hits: usize,
    /// Tasks that successfully ran a transform (excludes cache hits).
    /// Used by the header to show real build progress separately from
    /// the cache-promotion noise that would otherwise pin the cache
    /// percentage at 100% during the early phases of a mostly-cached
    /// build.
    pub transforms_finished: usize,
    /// Failed task addresses, in arrival order.
    pub failed: Vec<Addr>,
    /// True after a [`ConsoleEvent::BuildFinished`] has been observed.
    pub done: bool,
    /// Final overall success flag (only meaningful when `done == true`).
    pub ok: bool,
    /// Top-level target address from [`ConsoleEvent::BuildStarted`].
    pub root: Option<Addr>,
    /// Edo binary version captured from `SessionStarted`. Surfaced in
    /// the final summary so the JSONL log records which build emitted
    /// the result.
    pub edo_version: Option<String>,
    /// Project root from `ProjectLoaded`. Used by the renderer for the
    /// pre-build header line and by the post-build summary.
    pub project_root: Option<String>,
    /// Wall-clock build duration from `BuildFinished`, in milliseconds.
    /// Populated only after the build completes; the canvas reads this
    /// for the final summary line.
    pub elapsed_ms: u64,
    /// Total number of farms whose `setup()` will run, captured from
    /// `EnvSetupStarted`. Zero before that event arrives.
    pub env_setup_total: usize,
    /// Number of farms whose `setup()` has returned (success or
    /// failure). When `env_setup_total > 0 && !env_setup_done`, the
    /// header renders a pre-build progress line.
    pub env_setup_finished: usize,
    /// Currently-running farm setups, in arrival order. Used by the
    /// header so the user can see which farm is the bottleneck (the
    /// container case in particular: image pull + engine load).
    pub env_setup_active: Vec<Addr>,
    /// True after `EnvSetupFinished`. The header switches back to the
    /// generic pre-build shape (or scheduler-driven shape once
    /// `BuildStarted` arrives).
    pub env_setup_done: bool,
}

impl BuildState {
    /// Creates an empty state. Equivalent to `Default::default`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event into the state.
    ///
    /// Idempotent only on a per-event basis; replaying the same
    /// `NodeFinished` twice would double-count `finished`. The
    /// scheduler emits each event exactly once.
    pub fn apply(&mut self, event: &ConsoleEvent) {
        match event {
            ConsoleEvent::SessionStarted { edo_version, .. } => {
                self.edo_version = Some(edo_version.clone());
            }
            ConsoleEvent::ProjectLoaded { root, .. } => {
                self.project_root = Some(root.clone());
            }
            ConsoleEvent::EnvSetupStarted { total } => {
                self.env_setup_total = *total;
                self.env_setup_finished = 0;
                self.env_setup_active.clear();
                self.env_setup_done = false;
            }
            ConsoleEvent::EnvSetupFarmStarted { addr } => {
                if !self.env_setup_active.iter().any(|a| a == addr) {
                    self.env_setup_active.push(addr.clone());
                }
            }
            ConsoleEvent::EnvSetupFarmFinished { addr, .. } => {
                self.env_setup_active.retain(|a| a != addr);
                self.env_setup_finished += 1;
            }
            ConsoleEvent::EnvSetupFinished { .. } => {
                self.env_setup_done = true;
                self.env_setup_active.clear();
            }
            ConsoleEvent::BuildStarted { root, total } => {
                self.root = Some(root.clone());
                self.total = *total;
            }
            ConsoleEvent::NodeQueued { addr, id } => {
                self.active
                    .entry(addr.clone())
                    .or_insert_with(|| ActiveTask {
                        phase: None,
                        started: Instant::now(),
                        id: id.clone(),
                    });
            }
            ConsoleEvent::NodeCacheHit { addr, id: _ } => {
                self.active.remove(addr);
                self.finished += 1;
                self.cache_hits += 1;
            }
            ConsoleEvent::NodePhase { addr, phase } => {
                let entry = self
                    .active
                    .entry(addr.clone())
                    .or_insert_with(|| ActiveTask {
                        phase: None,
                        started: Instant::now(),
                        id: None,
                    });
                entry.phase = Some(*phase);
            }
            ConsoleEvent::NodeFinished { addr, ok, .. } => {
                self.active.remove(addr);
                self.finished += 1;
                self.transforms_finished += 1;
                if !ok {
                    self.failed.push(addr.clone());
                }
            }
            ConsoleEvent::Diagnostic { .. } => {
                // Diagnostics are scrolled, not aggregated.
            }
            ConsoleEvent::BuildFinished {
                ok,
                failed,
                elapsed_ms,
            } => {
                self.done = true;
                self.ok = *ok;
                self.elapsed_ms = *elapsed_ms;
                if self.failed.is_empty() {
                    self.failed = failed.clone();
                }
            }
        }
    }

    /// Cache hit ratio over **all finished work** (cache hits divided
    /// by `finished`). Returns 0 when nothing has finished.
    ///
    /// Note: in mostly-cached builds this pegs at 100% during the
    /// fetch phase because `fetch` promotes every cache hit to
    /// `finished` before any real transform runs. The header should
    /// pair this with [`Self::transforms_finished`] /
    /// [`Self::transforms_pending`] for an honest progress signal.
    pub fn cache_ratio(&self) -> f64 {
        if self.finished == 0 {
            return 0.0;
        }
        self.cache_hits as f64 / self.finished as f64
    }

    /// Count of nodes that are fetched and queued for the transform
    /// pool but haven't started running. Surfaced separately in the
    /// header so the user can see queue depth without those nodes
    /// rotating through the active-task table.
    pub fn waiting(&self) -> usize {
        self.active
            .values()
            .filter(|t| matches!(t.phase, Some(super::event::Phase::Wait)))
            .count()
    }

    /// Count of active tasks that are *actually doing something* \u2014
    /// either actively fetching (holds a fetch-semaphore permit) or
    /// past the post-fetch `Wait` phase. Pre-phase queued and
    /// `Wait`-parked nodes are excluded so the header doesn't claim a
    /// bogus parallelism count.
    pub fn active_running(&self) -> usize {
        self.active
            .values()
            .filter(|t| match t.phase {
                None | Some(super::event::Phase::Wait) => false,
                Some(_) => true, // Fetch + every transform-lifecycle phase
            })
            .count()
    }

    /// Count of nodes that need a real transform run but haven't
    /// finished yet (post-`BuildStarted`, excludes cache hits).
    pub fn transforms_pending(&self) -> usize {
        self.total
            .saturating_sub(self.cache_hits)
            .saturating_sub(self.transforms_finished)
    }
}

#[cfg(test)]
mod tests {
    use super::super::event::{ConsoleEvent, Phase, Severity};
    use super::*;
    use crate::storage::Id;

    fn id_for(name: &str) -> Id {
        Id::builder()
            .name(name.to_string())
            .digest("d".to_string())
            .build()
    }

    #[test]
    fn cache_hit_increments_finished_and_cache_hits() {
        let mut s = BuildState::new();
        let a = Addr::parse("//a").unwrap();
        s.apply(&ConsoleEvent::BuildStarted {
            root: a.clone(),
            total: 1,
        });
        s.apply(&ConsoleEvent::NodeQueued {
            addr: a.clone(),
            id: Some(id_for("a")),
        });
        s.apply(&ConsoleEvent::NodeCacheHit {
            addr: a.clone(),
            id: id_for("a"),
        });
        assert_eq!(s.finished, 1);
        assert_eq!(s.cache_hits, 1);
        assert!(s.active.is_empty());
        assert!((s.cache_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn phase_updates_active_task_in_place() {
        let mut s = BuildState::new();
        let a = Addr::parse("//a").unwrap();
        s.apply(&ConsoleEvent::NodeQueued {
            addr: a.clone(),
            id: None,
        });
        s.apply(&ConsoleEvent::NodePhase {
            addr: a.clone(),
            phase: Phase::Execute,
        });
        assert_eq!(s.active.get(&a).unwrap().phase, Some(Phase::Execute));
    }

    #[test]
    fn finished_failure_is_recorded() {
        let mut s = BuildState::new();
        let a = Addr::parse("//a").unwrap();
        s.apply(&ConsoleEvent::NodeQueued {
            addr: a.clone(),
            id: None,
        });
        s.apply(&ConsoleEvent::NodeFinished {
            addr: a.clone(),
            ok: false,
            elapsed_ms: 10,
        });
        assert_eq!(s.failed, vec![a.clone()]);
        assert_eq!(s.finished, 1);
        assert_eq!(s.cache_hits, 0);
        assert!(s.active.is_empty());
    }

    #[test]
    fn diagnostic_does_not_change_counters() {
        let mut s = BuildState::new();
        s.apply(&ConsoleEvent::diag(Severity::Info, "test", "hi"));
        assert_eq!(s.finished, 0);
        assert_eq!(s.cache_hits, 0);
        assert!(!s.done);
    }

    #[test]
    fn build_finished_marks_done_and_keeps_failed_list() {
        let mut s = BuildState::new();
        let a = Addr::parse("//a").unwrap();
        s.apply(&ConsoleEvent::NodeQueued {
            addr: a.clone(),
            id: None,
        });
        s.apply(&ConsoleEvent::NodeFinished {
            addr: a.clone(),
            ok: false,
            elapsed_ms: 10,
        });
        s.apply(&ConsoleEvent::BuildFinished {
            ok: false,
            elapsed_ms: 20,
            failed: vec![a.clone()],
        });
        assert!(s.done);
        assert!(!s.ok);
        assert_eq!(s.failed, vec![a]);
    }

    #[test]
    fn cache_hit_cascade_promotes_subtree() {
        // BuildState is renderer-input only — but the cascade case is the
        // most important one: many cache hits in a row before any
        // execute-phase events. Verify counters track correctly.
        let mut s = BuildState::new();
        for i in 0..5 {
            let a = Addr::parse(&format!("//cascade/n{i}")).unwrap();
            s.apply(&ConsoleEvent::NodeQueued {
                addr: a.clone(),
                id: Some(id_for(&format!("n{i}"))),
            });
            s.apply(&ConsoleEvent::NodeCacheHit {
                addr: a.clone(),
                id: id_for(&format!("n{i}")),
            });
        }
        assert_eq!(s.finished, 5);
        assert_eq!(s.cache_hits, 5);
        assert!(s.active.is_empty());
    }
}
