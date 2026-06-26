use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
};

use jiff::Timestamp;
use tokio::sync::oneshot;

use crate::{
    context::Addr,
    ui::{
        PromptChoice, PromptRequest,
        action::{Action, Task, TaskStatus},
    },
};

/// Aggregate of the current state of the running progress.
///
/// Invariants maintained across `apply`:
/// - `finished` is bumped exactly once for every task that reaches a
///   terminal status (`Cached` | `Success` | `Failed` | `Canceled`). On
///   `BuildFinish` the status bar reads `done/total` where
///   `done == finished`.
/// - `failed` is a supplementary list of the *names* of failing tasks
///   used to render the summary; it is a subset of `finished` (never
///   additive to it).
/// - `active` holds only tasks whose current status is a non-terminal
///   state (`Wait`, `Running`). Cached / Success / Failed / Cancelled
///   tasks are removed so a run over N cache-hit nodes doesn't leave N
///   entries in the map.
#[derive(Default)]
pub struct State {
    /// Root address used
    pub addr: Option<Addr>,
    /// Currently in-flight tasks keyed by component:id
    pub active: BTreeMap<String, Task>,
    /// Total transforms count
    pub total: usize,
    /// Tasks waiting for execution
    pub waiting: usize,
    /// Tasks that are in-flight
    pub in_flight: usize,
    /// Tasks that have reached a terminal state (success + cached +
    /// failed + cancelled). Displayed as `done` in the status bar.
    pub finished: usize,
    /// failed tasks
    pub failed: Vec<String>,
    /// True after finish
    pub done: bool,
    /// Final overall success flag
    pub ok: bool,
    /// Start time
    pub start: Timestamp,
    /// The prompt currently displayed on the statusline overlay, if
    /// any. When resolved (user picks a terminal choice, or a Ctrl+C
    /// dismisses it), `advance_prompt` promotes the next queued prompt
    /// into this slot.
    pub prompt: Option<Prompt>,
    /// Prompts arrived-but-not-yet-shown. Concurrent transform failures
    /// (common under the parallel scheduler) push into this queue so
    /// no request is silently auto-quit — the user sees them one at a
    /// time, in arrival order.
    pub prompt_queue: VecDeque<Prompt>,
    /// Insertion-ordered list of `component:id` keys for tasks whose
    /// current status is `Running`. Maintained incrementally by `apply`:
    ///
    /// - Pushed when a task enters `Running` (from `StartTask(Running)`,
    ///   or from `UpdateTask(_ -> Running)`).
    /// - Removed when a running task leaves `Running` (to Wait or to
    ///   any terminal status).
    ///
    /// The vec avoids an O(N log N) sort of `active` on every frame —
    /// `render_tasks` iterates this slice directly. The BTreeMap
    /// `active` remains the point-lookup index.
    pub running_order: Vec<String>,
    /// True when state has been mutated since the last frame flush.
    /// Cleared by the frame tick after `terminal.draw`. Consumers must
    /// not read this outside of `App` — it is an internal render hint,
    /// not a semantic flag.
    pub dirty: bool,
}

pub struct Prompt {
    /// Address of the failed transform
    pub addr: Addr,
    /// Stringified error message
    pub error: String,
    /// Optional path to the per-task `.log` file
    pub log_file: Option<PathBuf>,
    /// Whether retry is offered
    pub allow_retry: bool,
    /// Whether the user can drop into a shell inside the failed env
    pub allow_shell: bool,
    /// Shell callback. Invoked from the UI task when the user picks `s`.
    pub shell: Option<Box<dyn FnMut() -> std::io::Result<()> + Send>>,
    /// The oneshot responder attached to the request. Taken when the
    /// user picks a terminal choice; consumers of `state.prompt` must
    /// treat this as the single-source-of-truth for resolving the
    /// awaiting `Console::prompt` caller.
    pub responder: Option<oneshot::Sender<PromptChoice>>,
    /// Which option the user has selected
    pub selected: PromptSelect,
}

impl Prompt {
    /// Resolve the pending prompt with `choice` and consume the responder.
    /// Idempotent — a second call is a no-op.
    pub fn resolve(&mut self, choice: PromptChoice) {
        if let Some(tx) = self.responder.take() {
            let _ = tx.send(choice);
        }
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSelect {
    #[default]
    ViewLog,
    Retry,
    Shell,
    Quit,
}

impl State {
    /// Undo the counter contribution of a task's previous non-terminal
    /// status, so `StartTask` / `UpdateTask` on an already-tracked key
    /// doesn't double-bump `waiting` or `in_flight`. Called before
    /// applying the new status.
    fn subtract_prev_counter(waiting: &mut usize, in_flight: &mut usize, prev: Option<TaskStatus>) {
        match prev {
            Some(TaskStatus::Wait) => *waiting = waiting.saturating_sub(1),
            Some(TaskStatus::Running) => *in_flight = in_flight.saturating_sub(1),
            _ => {}
        }
    }

    /// Attach a prompt to the state. Called by the App when a
    /// `PromptEnvelope` arrives on the prompt channel. If a prompt is
    /// already displayed the new request is enqueued and shown as soon
    /// as the current prompt is resolved. No request is silently
    /// discarded — concurrent transform failures under the parallel
    /// scheduler are surfaced one at a time to the user.
    pub fn set_prompt(&mut self, request: PromptRequest, responder: oneshot::Sender<PromptChoice>) {
        self.dirty = true;
        // Pick a sensible default selection: prefer ViewLog when a log
        // is available, otherwise Retry / Shell / Quit in that order.
        let selected = if request.log_file.is_some() {
            PromptSelect::ViewLog
        } else if request.allow_retry {
            PromptSelect::Retry
        } else if request.allow_shell {
            PromptSelect::Shell
        } else {
            PromptSelect::Quit
        };
        // Sanity check (release-safe: on selection sets built above the
        // invariant holds by construction — the check catches future
        // refactors that add a new PromptSelect variant).
        debug_assert!(
            Self::selection_is_enabled(&request, selected),
            "default selection {:?} is not enabled by the request",
            selected
        );
        let prompt = Prompt {
            addr: request.addr,
            error: request.error,
            log_file: request.log_file,
            allow_retry: request.allow_retry,
            allow_shell: request.allow_shell,
            shell: request.shell,
            responder: Some(responder),
            selected,
        };
        if self.prompt.is_none() {
            self.prompt = Some(prompt);
        } else {
            self.prompt_queue.push_back(prompt);
        }
    }

    /// After the current prompt is resolved, promote the next queued
    /// prompt (if any) into the visible slot. Idempotent — returns
    /// `false` when the queue is empty.
    pub fn advance_prompt(&mut self) -> bool {
        if let Some(next) = self.prompt_queue.pop_front() {
            self.prompt = Some(next);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Drain the entire prompt queue, resolving each with `Quit`.
    /// Called on shutdown so no caller of `Console::prompt` is left
    /// awaiting a dropped oneshot.
    pub fn drain_prompts_with_quit(&mut self) {
        if let Some(mut p) = self.prompt.take() {
            p.resolve(PromptChoice::Quit);
        }
        for mut p in std::mem::take(&mut self.prompt_queue) {
            p.resolve(PromptChoice::Quit);
        }
    }

    /// Check whether a `PromptSelect` variant is offered by a
    /// `PromptRequest`. Used by the `debug_assert!` in `set_prompt` and
    /// by future callers that want to validate selection changes.
    fn selection_is_enabled(request: &PromptRequest, sel: PromptSelect) -> bool {
        match sel {
            PromptSelect::ViewLog => request.log_file.is_some(),
            PromptSelect::Shell => request.allow_shell,
            PromptSelect::Retry => request.allow_retry,
            PromptSelect::Quit => true,
        }
    }

    pub fn apply(&mut self, action: &Action) {
        // Every action other than `Terminate` mutates observable state
        // (progress counters, active map, running_order, prompt slot).
        // Setting `dirty` once here is cheaper and less error-prone
        // than sprinkling it across each arm.
        if !matches!(action, Action::Terminate) {
            self.dirty = true;
        }
        match action {
            Action::Header {
                addr, started_at, ..
            } => {
                self.addr = addr.clone();
                self.start = *started_at;
            }
            Action::StartBuild { addr, total } => {
                // Take the max: multiple `StartBuild` in one session (a
                // re-run, or a sub-graph emission bug) must not double
                // the denominator. `total = state.total.max(new_total)`
                // keeps the status bar sensible without silently
                // clobbering a legitimate expansion.
                self.total = self.total.max(*total);
                self.addr = Some(addr.clone());
            }
            Action::StartTask {
                component,
                id,
                status,
                operation,
                message,
            } => {
                let key = format!("{component}:{id}");

                // Look up the previous status *first* so a re-emit of
                // the same key (e.g. Wait then Running from different
                // scheduler phases) doesn't double-count. This is the
                // same discipline `UpdateTask` uses below: consult
                // `active` before mutating counters.
                let prev_status = self.active.get(&key).map(|t| t.status);
                Self::subtract_prev_counter(&mut self.waiting, &mut self.in_flight, prev_status);

                // Cached tasks are already terminal: bump `finished`
                // and DO NOT insert into `active`. Previously the
                // insert leaked entries for every cache hit,
                // consuming O(N) memory on cache-warm runs.
                match status {
                    TaskStatus::Cached => {
                        self.finished += 1;
                        // If this key was previously tracked as
                        // Running/Wait, remove it — Cached is terminal.
                        if prev_status.is_some() {
                            self.active.remove(&key);
                            self.running_order.retain(|k| k != &key);
                        }
                        return;
                    }
                    TaskStatus::Failed => {
                        self.finished += 1;
                        self.failed.push(key.clone());
                    }
                    TaskStatus::Success => {
                        self.finished += 1;
                    }
                    TaskStatus::Running => self.in_flight += 1,
                    TaskStatus::Wait => self.waiting += 1,
                    TaskStatus::Canceled => {
                        // Cancellation is terminal; counts as finished
                        // so `done/total` converges at end.
                        self.finished += 1;
                    }
                };
                // Only non-terminal statuses reach here (Wait/Running):
                // terminal statuses are handled above and short-circuit
                // via `return` for Cached, or fall through only when
                // the task should be tracked. Success/Failed/Cancelled
                // on `StartTask` are ephemeral and we don't retain them.
                if matches!(status, TaskStatus::Wait | TaskStatus::Running) {
                    let prev_was_running = prev_status == Some(TaskStatus::Running);
                    self.active.insert(
                        key.clone(),
                        Task::builder()
                            .component(component.clone())
                            .id(id.clone())
                            .status(*status)
                            .operation(operation.clone())
                            .maybe_message(message.clone())
                            .status_since(Timestamp::now())
                            .build(),
                    );
                    match (*status, prev_was_running) {
                        (TaskStatus::Running, false) => {
                            self.running_order.push(key);
                        }
                        (TaskStatus::Wait, true) => {
                            self.running_order.retain(|k| k != &key);
                        }
                        _ => {}
                    }
                } else {
                    // Terminal statuses (Success/Failed/Cancelled) via
                    // StartTask are treated as one-shot events: no
                    // insertion into `active`, and any prior entry is
                    // dropped since the task is now terminal.
                    if prev_status.is_some() {
                        self.active.remove(&key);
                        self.running_order.retain(|k| k != &key);
                    }
                }
            }
            Action::UpdateTask {
                component,
                id,
                operation,
                status,
                message,
            } => {
                let key = format!("{component}:{id}");
                let mut should_remove = false;
                let mut push_running = false;
                let mut remove_running = false;
                if let Some(task) = self.active.get_mut(&key) {
                    match (&task.status, status) {
                        (TaskStatus::Wait, TaskStatus::Running) => {
                            self.waiting = self.waiting.saturating_sub(1);
                            self.in_flight += 1;
                            push_running = true;
                        }
                        // `Running -> Wait` is emitted by graph.rs when
                        // `prepare` finishes and the node is queued for
                        // a transform worker. Without this arm, the
                        // `in_flight` counter stays bumped and the
                        // `waiting` counter never grows, so the status
                        // bar showed too many active and too few
                        // waiting.
                        (TaskStatus::Running, TaskStatus::Wait) => {
                            self.in_flight = self.in_flight.saturating_sub(1);
                            self.waiting += 1;
                            remove_running = true;
                        }
                        (TaskStatus::Wait, TaskStatus::Success) => {
                            self.waiting = self.waiting.saturating_sub(1);
                            self.finished += 1;
                            should_remove = true;
                        }
                        (TaskStatus::Wait, TaskStatus::Failed) => {
                            self.waiting = self.waiting.saturating_sub(1);
                            self.finished += 1;
                            should_remove = true;
                            self.failed.push(key.clone());
                        }
                        (TaskStatus::Wait, TaskStatus::Canceled) => {
                            self.waiting = self.waiting.saturating_sub(1);
                            self.finished += 1;
                            should_remove = true;
                        }
                        (TaskStatus::Running, TaskStatus::Success) => {
                            self.in_flight = self.in_flight.saturating_sub(1);
                            self.finished += 1;
                            should_remove = true;
                            remove_running = true;
                        }
                        (TaskStatus::Running, TaskStatus::Failed) => {
                            self.in_flight = self.in_flight.saturating_sub(1);
                            self.finished += 1;
                            should_remove = true;
                            remove_running = true;
                            self.failed.push(key.clone());
                        }
                        (TaskStatus::Running, TaskStatus::Canceled) => {
                            self.in_flight = self.in_flight.saturating_sub(1);
                            // Cancelled counts as finished so the
                            // status bar converges at build end.
                            self.finished += 1;
                            should_remove = true;
                            remove_running = true;
                        }

                        _ => {}
                    }
                    task.operation = operation.clone();
                    task.status = *status;
                    task.status_since = Timestamp::now();
                    task.message = message.clone();
                }
                if should_remove {
                    self.active.remove(&key);
                }
                if push_running {
                    // Idempotent push (a Wait -> Running transition on
                    // a task already tracked as running is a no-op).
                    if !self.running_order.iter().any(|k| k == &key) {
                        self.running_order.push(key.clone());
                    }
                }
                if remove_running {
                    self.running_order.retain(|k| k != &key);
                }
            }
            Action::BuildFinish => {
                self.done = true;
                self.ok = self.failed.is_empty();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(comp: &str, id: &str, status: TaskStatus) -> Action {
        Action::StartTask {
            component: comp.to_string(),
            id: id.to_string(),
            status,
            operation: "op".to_string(),
            message: None,
        }
    }

    fn update(comp: &str, id: &str, status: TaskStatus) -> Action {
        Action::UpdateTask {
            component: comp.to_string(),
            id: id.to_string(),
            operation: "op".to_string(),
            status,
            message: None,
        }
    }

    #[test]
    fn start_task_wait_populates_active_and_waiting() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Wait));
        assert_eq!(s.waiting, 1);
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 0);
        assert_eq!(s.active.len(), 1);
    }

    #[test]
    fn start_task_running_populates_active_and_in_flight() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Running));
        assert_eq!(s.in_flight, 1);
        assert_eq!(s.active.len(), 1);
    }

    #[test]
    fn start_task_cached_does_not_leak_active() {
        let mut s = State::default();
        for i in 0..50 {
            s.apply(&start("t", &i.to_string(), TaskStatus::Cached));
        }
        assert_eq!(s.finished, 50);
        assert_eq!(s.active.len(), 0, "cached tasks must not populate active");
    }

    #[test]
    fn wait_to_running() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Wait));
        s.apply(&update("t", "1", TaskStatus::Running));
        assert_eq!(s.waiting, 0);
        assert_eq!(s.in_flight, 1);
    }

    #[test]
    fn running_to_wait_prepare_finished() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Running));
        s.apply(&update("t", "1", TaskStatus::Wait));
        assert_eq!(s.in_flight, 0, "running->wait must decrement in_flight");
        assert_eq!(s.waiting, 1, "running->wait must increment waiting");
    }

    #[test]
    fn wait_to_success() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Wait));
        s.apply(&update("t", "1", TaskStatus::Success));
        assert_eq!(s.waiting, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn wait_to_failed() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Wait));
        s.apply(&update("t", "1", TaskStatus::Failed));
        assert_eq!(s.waiting, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn wait_to_cancelled() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Wait));
        s.apply(&update("t", "1", TaskStatus::Canceled));
        assert_eq!(s.waiting, 0);
        assert_eq!(
            s.finished, 1,
            "cancelled counts as finished for status-bar convergence"
        );
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn running_to_success() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Running));
        s.apply(&update("t", "1", TaskStatus::Success));
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn running_to_failed() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Running));
        s.apply(&update("t", "1", TaskStatus::Failed));
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn running_to_cancelled() {
        let mut s = State::default();
        s.apply(&start("t", "1", TaskStatus::Running));
        s.apply(&update("t", "1", TaskStatus::Canceled));
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn start_build_uses_max_not_sum() {
        let mut s = State::default();
        let addr = Addr::parse("//project/target").unwrap();
        s.apply(&Action::StartBuild {
            addr: addr.clone(),
            total: 5,
        });
        s.apply(&Action::StartBuild {
            addr: addr.clone(),
            total: 5,
        });
        // Same-total re-emit: max keeps `total` at 5, not 10.
        assert_eq!(s.total, 5, "repeated same-total StartBuild must not double");
        s.apply(&Action::StartBuild { addr, total: 12 });
        assert_eq!(s.total, 12, "larger StartBuild expands to that size");
    }

    #[test]
    fn start_task_reemit_does_not_double_count() {
        // Regression test for the review finding: `StartTask(Wait)`
        // followed by `StartTask(Running)` for the same key must not
        // leave `waiting == 1` and `in_flight == 1` — the second
        // `StartTask` supersedes the first.
        let mut s = State::default();
        s.apply(&start("t", "a", TaskStatus::Wait));
        assert_eq!(s.waiting, 1);
        assert_eq!(s.in_flight, 0);
        s.apply(&start("t", "a", TaskStatus::Running));
        assert_eq!(s.waiting, 0, "second StartTask must un-bump waiting");
        assert_eq!(s.in_flight, 1);
        s.apply(&update("t", "a", TaskStatus::Success));
        assert_eq!(s.waiting, 0);
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.active.len(), 0);
    }

    #[test]
    fn start_task_reemit_wait_twice_does_not_double_count() {
        let mut s = State::default();
        s.apply(&start("t", "a", TaskStatus::Wait));
        s.apply(&start("t", "a", TaskStatus::Wait));
        assert_eq!(s.waiting, 1, "two Wait StartTasks must not double");
    }

    #[test]
    fn start_task_reemit_running_then_terminal_removes_from_running_order() {
        let mut s = State::default();
        s.apply(&start("t", "a", TaskStatus::Running));
        assert_eq!(s.running_order.len(), 1);
        // Re-emit with a terminal status via StartTask (rare but
        // observable): task should leave running_order.
        s.apply(&start("t", "a", TaskStatus::Cached));
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.finished, 1);
        assert_eq!(s.active.len(), 0);
        assert!(s.running_order.is_empty());
    }

    #[test]
    fn counters_converge_at_build_finish() {
        let mut s = State::default();
        let addr = Addr::parse("//project/target").unwrap();
        s.apply(&Action::StartBuild { addr, total: 4 });
        // 1 cached
        s.apply(&start("t", "cached", TaskStatus::Cached));
        // 1 success via Running -> Success
        s.apply(&start("t", "ok", TaskStatus::Running));
        s.apply(&update("t", "ok", TaskStatus::Success));
        // 1 failed via Running -> Failed
        s.apply(&start("t", "boom", TaskStatus::Running));
        s.apply(&update("t", "boom", TaskStatus::Failed));
        // 1 cancelled via Running -> Cancelled
        s.apply(&start("t", "gone", TaskStatus::Running));
        s.apply(&update("t", "gone", TaskStatus::Canceled));

        s.apply(&Action::BuildFinish);

        assert_eq!(s.in_flight, 0);
        assert_eq!(s.waiting, 0);
        assert_eq!(s.finished, 4, "expected 4 terminal transitions");
        assert_eq!(s.failed.len(), 1);
        assert!(s.done);
        assert!(!s.ok, "one failure keeps ok=false");
    }
}
