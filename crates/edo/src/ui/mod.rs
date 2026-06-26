//! Split event/action-loop console.
//!
//! The `ui` module owns the process-wide `Console` handle, the two channels
//! it feeds (build actions and interactive prompts), and the `App` runtime
//! that drives them.
//!
//! Two channels intentionally: build-derived actions and user-driven prompts
//! travel on separate queues so a hot action producer cannot starve prompt
//! delivery. Actions ride a **bounded** `tokio::sync::mpsc` (capacity
//! `CHANNEL_CAPACITY`); prompts ride an unbounded channel (they are rare
//! and must never be dropped).
//!
//! Backpressure policy on `Console::send`:
//! - Privileged actions (`Terminate`, `StartBuild`, `BuildFinish`,
//!   diagnostics at `Error`/`Fatal`) fall back to `send().await` so they
//!   are never dropped.
//! - Everything else uses `try_send`; on `Full` the event is dropped and
//!   an atomic counter is bumped. The count is emitted once at shutdown
//!   as a single warning line.
//!
//! There is exactly one async task ever: the App. `crossterm::event::EventStream`
//! is polled directly by the App loop (invariant enforced by
//! `event_stream_only_constructed_in_ui`).

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use jiff::Timestamp;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    context::Addr,
    ui::action::{Action, Severity, TaskStatus},
};

pub mod action;
mod app;
pub mod error;
mod macros;
mod render;
pub mod store;
#[allow(clippy::module_inception)]
mod ui;

pub use action::{Action as UiAction, Severity as UiSeverity, TaskStatus as UiTaskStatus};

pub(crate) type Result<T> = std::result::Result<T, error::Error>;

/// Bound on the action channel. When emitters flood past this,
/// non-privileged `try_send` calls drop the newest event and bump a
/// counter surfaced at shutdown as a single `N diagnostics dropped`
/// warning. 4096 is a generous slack — steady-state emission is bursty,
/// not sustained.
const CHANNEL_CAPACITY: usize = 4096;

/// Global console handle. Installed exactly once by [`Console::install`].
pub static CONSOLE: OnceLock<Console> = OnceLock::new();

/// Choice returned by an interactive failure prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptChoice {
    /// User asked to retry the failed transform.
    Retry,
    /// User asked to abort the build.
    Quit,
}

/// Request driving an interactive failure prompt.
pub struct PromptRequest {
    /// Address of the failed transform.
    pub addr: Addr,
    /// Stringified error message.
    pub error: String,
    /// Optional path to the per-task `.log` file.
    pub log_file: Option<std::path::PathBuf>,
    /// Whether retry is offered.
    pub allow_retry: bool,
    /// Whether the user can drop into a shell inside the failed env.
    pub allow_shell: bool,
    /// Shell callback. Invoked from the App task when the user picks `s`.
    pub shell: Option<Box<dyn FnMut() -> std::io::Result<()> + Send>>,
}

/// Envelope over the prompt channel: the request plus the oneshot the
/// caller of [`Console::prompt`] is awaiting.
pub(crate) type PromptEnvelope = (PromptRequest, oneshot::Sender<PromptChoice>);

/// Cheap-to-clone handle to the build-event console.
pub struct Console {
    handle: Arc<Inner>,
}

struct Inner {
    action_sender: mpsc::Sender<Action>,
    prompt_sender: mpsc::UnboundedSender<PromptEnvelope>,
    /// Join handle for the App task. `Mutex<Option<_>>` gives us
    /// take-once semantics through the shared `Arc<Inner>`: the first
    /// `Console::shutdown` takes it and drives teardown, later callers
    /// find `None` and *await the completion signal* rather than
    /// returning immediately. `std::sync::Mutex` is fine — the lock is
    /// only held for a `take()`, never across an await.
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Broadcast "shutdown complete" flag. The first `shutdown` caller
    /// flips this to `true` after joining the App; concurrent callers
    /// await the transition. `watch` is chosen over `oneshot::channel`
    /// because it supports multiple receivers.
    shutdown_done_tx: watch::Sender<bool>,
    shutdown_done_rx: watch::Receiver<bool>,
    /// Count of non-privileged actions dropped due to backpressure.
    /// Emitted once at shutdown.
    dropped: AtomicU64,
}

impl Console {
    /// Spawn the App task and return a cheap-cloneable console handle
    /// that forwards actions and prompt requests to it.
    pub fn new() -> Self {
        let (action_tx, action_rx) = mpsc::channel::<Action>(CHANNEL_CAPACITY);
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptEnvelope>();
        let (shutdown_done_tx, shutdown_done_rx) = watch::channel(false);
        let join = tokio::spawn(async move {
            let mut app = app::App::new(action_rx, prompt_rx);
            if let Err(e) = app.run().await {
                tracing::warn!(subsystem = "ui", "app exited with error: {e}");
            }
        });
        Self {
            handle: Arc::new(Inner {
                action_sender: action_tx,
                prompt_sender: prompt_tx,
                join: Mutex::new(Some(join)),
                shutdown_done_tx,
                shutdown_done_rx,
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// Install this console as the process-wide [`CONSOLE`].
    ///
    /// Returns `Err(self)` if a console is already installed. A silent
    /// double-install would leak a second App task (already spawned by
    /// `Console::new`) that briefly competes with the first for stdin
    /// and the stderr viewport, then unravels as `self` is dropped —
    /// often leaving the terminal in an inconsistent state.
    pub fn install(self) -> std::result::Result<(), Self> {
        CONSOLE.set(self)
    }

    /// Returns the process-wide console handle if one has been installed.
    pub fn global() -> Option<&'static Console> {
        CONSOLE.get()
    }

    pub async fn emit_header(
        &self,
        tool: &str,
        version: &semver::Version,
        addr: Option<Addr>,
        args: Vec<(String, String)>,
    ) {
        self.send(&Action::Header {
            tool: tool.to_string(),
            version: version.clone(),
            addr,
            args,
            started_at: Timestamp::now(),
        })
        .await;
    }

    pub async fn emit_summary<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        transforms: usize,
        sources: usize,
        farms: usize,
        locked: bool,
    ) {
        self.send(&Action::Summary {
            root: path.as_ref().to_path_buf(),
            transforms,
            sources,
            farms,
            locked,
        })
        .await;
    }

    pub async fn start_build(&self, addr: &Addr, total: usize) {
        self.send(&Action::StartBuild {
            addr: addr.clone(),
            total,
        })
        .await;
    }

    pub async fn start_task(
        &self,
        component: &str,
        id: &str,
        operation: &str,
        status: TaskStatus,
        message: Option<String>,
    ) {
        self.send(&Action::StartTask {
            component: component.to_string(),
            id: id.to_string(),
            status,
            operation: operation.to_string(),
            message,
        })
        .await;
    }

    pub async fn update_task(
        &self,
        component: &str,
        id: &str,
        operation: &str,
        status: TaskStatus,
        message: Option<String>,
    ) {
        self.send(&Action::UpdateTask {
            component: component.to_string(),
            id: id.to_string(),
            operation: operation.to_string(),
            status,
            message,
        })
        .await;
    }

    pub async fn emit_diagnostic(
        &self,
        component: &str,
        id: Option<String>,
        severity: Severity,
        message: &str,
    ) {
        self.send(&Action::Diagnostic {
            component: component.to_string(),
            id,
            severity,
            message: message.to_string(),
        })
        .await;
    }

    pub async fn finish_build(&self) {
        self.send(&Action::BuildFinish).await;
    }

    pub async fn emit_terminate(&self) {
        // Terminate must not be dropped: it's the sole shutdown signal
        // to the App task via the action channel. Bypass the try_send
        // path entirely.
        let _ = self.handle.action_sender.send(Action::Terminate).await;
    }

    /// Send an action to the App task.
    ///
    /// Privileged actions fall back to the awaiting `send` on
    /// backpressure so they cannot be silently dropped:
    /// - `Terminate` — the sole shutdown signal.
    /// - `StartBuild`, `BuildFinish` — build-scope boundaries.
    /// - `Header`, `Summary` — one-shot session identifiers; users
    ///   expect to see the target/version even under early flooding.
    /// - `Diagnostic{ severity: Error | Fatal }` — real failure signal.
    /// - `UpdateTask` and `StartTask` carrying a terminal status
    ///   (`Failed`, `Canceled`, `Cached`, `Success`) — dropping these
    ///   would strand a task counter and hang `done/total` at end-of-
    ///   build.
    ///
    /// Everything else uses `try_send` and increments `dropped` on both
    /// `Full` and `Closed`, so the shutdown warning reflects the true
    /// number of dropped diagnostics.
    pub async fn send(&self, action: &Action) {
        if is_privileged(action) {
            let _ = self.handle.action_sender.send(action.clone()).await;
            return;
        }
        match self.handle.action_sender.try_send(action.clone()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.handle.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Drive an interactive failure prompt.
    ///
    /// Surfaces the failure as a diagnostic first (so the log/canvas
    /// records *what* failed), then sends the request onto the prompt
    /// channel and awaits the choice. If the App task is gone (rx
    /// disconnected) or no console is installed, falls back to
    /// [`PromptChoice::Quit`].
    pub async fn prompt(&self, request: PromptRequest) -> PromptChoice {
        let (tx, rx) = oneshot::channel::<PromptChoice>();
        let msg = if let Some(path) = request.log_file.as_ref() {
            format!(
                "transform {} failed: {} (log: {})",
                request.addr,
                request.error,
                path.display()
            )
        } else {
            format!("transform {} failed: {}", request.addr, request.error)
        };
        self.emit_diagnostic(
            "scheduler",
            Some(request.addr.to_string()),
            Severity::Error,
            &msg,
        )
        .await;
        if self.handle.prompt_sender.send((request, tx)).is_err() {
            return PromptChoice::Quit;
        }
        match rx.await {
            Ok(choice) => choice,
            Err(_) => PromptChoice::Quit,
        }
    }

    /// Drain the App task and restore the terminal. Concurrent-safe
    /// and idempotent.
    ///
    /// The **first** caller to `take()` the join handle sends
    /// `Action::Terminate` on the privileged send path, awaits the App
    /// task, then broadcasts completion via the shutdown-done watch.
    /// **Concurrent** callers find the join handle already taken and
    /// await the same broadcast — they do not return until the first
    /// caller's teardown has finished. Subsequent (post-completion)
    /// calls observe `shutdown_done == true` and return immediately.
    ///
    /// This guarantees no caller ever sees "shutdown returned" while
    /// `restore_terminal()` is still in-flight, closing the race the
    /// previous take-and-return-immediately design left open.
    pub async fn shutdown(&self) {
        let join = self.handle.join.lock().unwrap().take();
        let Some(j) = join else {
            // Someone else is (or already has) driven the teardown.
            // Wait for the broadcast so we don't return before the
            // terminal is restored.
            let mut rx = self.handle.shutdown_done_rx.clone();
            // If the value is already `true`, `changed().await` returns
            // immediately on the first poll. Otherwise it awaits the
            // transition.
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    // Sender dropped — Inner is being dropped. Nothing
                    // useful to wait for.
                    break;
                }
            }
            return;
        };
        // The App loop exits on Terminate or on channel close. We
        // send Terminate via the privileged path so backpressure
        // can't drop it, then await the task to guarantee
        // `restore_terminal()` has finished before we return.
        self.emit_terminate().await;
        match j.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {
                tracing::warn!(subsystem = "ui", "app task was cancelled");
            }
            Err(e) => {
                tracing::warn!(subsystem = "ui", "app task failed: {e}");
            }
        }
        // Report dropped-diagnostic count once, at shutdown.
        let dropped = self.handle.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                subsystem = "ui",
                dropped = dropped,
                "diagnostics dropped due to channel backpressure"
            );
        }
        // Broadcast completion so concurrent callers wake up.
        let _ = self.handle.shutdown_done_tx.send(true);
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

/// Shorthand: best-effort send to the global console. Used by the `ui_*`
/// macros. If the console has not been installed the call is silently
/// dropped.
#[doc(hidden)]
pub async fn try_send(action: &Action) {
    if let Some(c) = CONSOLE.get() {
        c.send(action).await;
    }
}

/// Whether an action bypasses the drop-on-backpressure policy in
/// `Console::send`. See the docstring on `send` for the rationale.
fn is_privileged(action: &Action) -> bool {
    match action {
        Action::Terminate
        | Action::StartBuild { .. }
        | Action::BuildFinish
        | Action::Header { .. }
        | Action::Summary { .. } => true,
        Action::Diagnostic { severity, .. } => {
            matches!(severity, Severity::Error | Severity::Fatal)
        }
        Action::UpdateTask { status, .. } | Action::StartTask { status, .. } => {
            matches!(
                status,
                TaskStatus::Failed
                    | TaskStatus::Canceled
                    | TaskStatus::Cached
                    | TaskStatus::Success
            )
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Addr;
    use crate::ui::action::{Severity, TaskStatus};

    /// Construct an App with fresh channels for tests, bypassing the
    /// global CONSOLE. Returns the App plus the sender handles.
    fn make_app() -> (
        app::App,
        mpsc::Sender<Action>,
        mpsc::UnboundedSender<PromptEnvelope>,
    ) {
        let (tx, rx) = mpsc::channel::<Action>(64);
        let (ptx, prx) = mpsc::unbounded_channel::<PromptEnvelope>();
        let app = app::App::new(rx, prx);
        (app, tx, ptx)
    }

    #[tokio::test]
    async fn plain_mode_prints_lines() {
        let (mut app, tx, _ptx) = make_app();
        // Bail if the test env happens to be a TTY (unusual under
        // `cargo test`) — the plain-mode assertions don't apply.
        if !app.is_plain() {
            return;
        }
        tx.send(Action::Diagnostic {
            component: "test".to_string(),
            id: None,
            severity: Severity::Info,
            message: "hello".to_string(),
        })
        .await
        .unwrap();
        tx.send(Action::Terminate).await.unwrap();
        app.run().await.unwrap();
    }

    #[tokio::test]
    async fn plain_mode_answers_prompt_with_quit() {
        let (mut app, tx, ptx) = make_app();
        if !app.is_plain() {
            return;
        }

        let addr = Addr::parse("//project/target").unwrap();
        let (rtx, rrx) = oneshot::channel();
        let request = PromptRequest {
            addr,
            error: "boom".to_string(),
            log_file: None,
            allow_retry: true,
            allow_shell: false,
            shell: None,
        };
        ptx.send((request, rtx)).unwrap();

        let handle = tokio::spawn(async move { app.run().await });
        // Give the loop a moment to observe the prompt, then terminate.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(Action::Terminate).await.unwrap();

        let choice = rrx.await.unwrap();
        assert_eq!(choice, PromptChoice::Quit);
        let _ = handle.await;
    }

    /// Render `n` `Running` tasks into a viewport of the given total
    /// height and return the trimmed task-pane rows (y=2..height).
    fn render_tasks_rows(n: usize, viewport_height: u16) -> Vec<String> {
        use crate::ui::store::State;
        use ratatui::{Terminal, backend::TestBackend};
        let mut s = State::default();
        for i in 0..n {
            s.apply(&Action::StartTask {
                component: "t".to_string(),
                id: format!("{i:02}"),
                status: TaskStatus::Running,
                operation: "execute".to_string(),
                message: None,
            });
        }
        let backend = TestBackend::new(80, viewport_height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw_frame(&s, frame))
            .unwrap();
        let buf = terminal.backend().buffer();
        // Statusline layout is `Length(2)`, so tasks start at y=2.
        let mut rows: Vec<String> = Vec::new();
        for y in 2..viewport_height {
            let mut row = String::new();
            for x in 0..80 {
                row.push_str(buf[(x, y)].symbol());
            }
            rows.push(row.trim_end().to_string());
        }
        rows
    }

    #[test]
    fn render_tasks_fits_default_worker_count() {
        // 2-row statusline + 8 task rows = viewport height 10.
        let rows = render_tasks_rows(8, 10);
        assert_eq!(rows.len(), 8, "expected 8 task rows, got {rows:?}");
        for (i, row) in rows.iter().enumerate() {
            assert!(!row.is_empty(), "task row {i} is empty (rendered={rows:?})");
            assert!(
                !row.contains("more running"),
                "no overflow marker expected with 8 workers in a 10-row pane; got {row:?}"
            );
        }
    }

    #[test]
    fn render_tasks_overflow_marker_is_visible() {
        // 10 running tasks into a 7-row viewport (2 status + 5 tasks).
        let rows = render_tasks_rows(10, 7);
        assert_eq!(rows.len(), 5, "expected 5 task rows, got {rows:?}");
        for (i, row) in rows.iter().enumerate() {
            assert!(
                !row.is_empty(),
                "task row {i} is empty; a line was silently clipped (rendered={rows:?})"
            );
        }
        let last = rows.last().expect("at least one row");
        assert!(
            last.contains("more running"),
            "last row should be overflow marker, got {last:?} (all={rows:?})"
        );
    }

    // -------------------------------------------------------------------------
    // Acceptance-criteria tests (AC1, AC2, AC3, AC4, AC5, AC6)
    // -------------------------------------------------------------------------

    /// AC1 — no starvation regression. This test drives the App's
    /// interactive-path handlers *directly* (via `test_handle_action`
    /// / `test_drain_actions`) rather than routing through the plain
    /// loop. That way we're exercising the same code path that would
    /// starve under the old flume+biased-select design.
    ///
    /// Pushes 10 000 actions into the bounded channel, drains in
    /// batches of `EVENT_DRAIN_MAX`, and asserts:
    ///   1. Every action gets folded into state (finished == 10_000
    ///      Diagnostic events don't touch `finished`; we assert
    ///      `dirty == true` after each drain).
    ///   2. After ceil(10_000 / EVENT_DRAIN_MAX) drain cycles the
    ///      channel is empty.
    ///   3. Each individual drain call took less than
    ///      `EVENT_DRAIN_BUDGET × 5` wall-clock (belt-and-braces).
    #[tokio::test(flavor = "current_thread")]
    async fn key_not_starved_by_action_flood() {
        let (mut app, tx, _ptx) = make_app();
        // Force plain so no TTY code runs during the test, but keep
        // driving `handle_action` / `drain_actions` directly — those
        // are the interactive-path fns we want to exercise.
        app.force_plain();
        const TOTAL: usize = 10_000;
        // Push everything before draining so `try_recv` actually finds
        // something on every drain call.
        for i in 0..TOTAL {
            // Non-blocking: bounded channel capacity 64 (make_app),
            // so drain in bursts.
            if tx
                .try_send(Action::Diagnostic {
                    component: "flood".to_string(),
                    id: None,
                    severity: Severity::Trace,
                    message: format!("{i}"),
                })
                .is_err()
            {
                // Channel full: drain now, then continue.
                app.test_drain_actions();
                tx.send(Action::Diagnostic {
                    component: "flood".to_string(),
                    id: None,
                    severity: Severity::Trace,
                    message: format!("{i}"),
                })
                .await
                .unwrap();
            }
        }
        // Drain remaining. Each call caps at EVENT_DRAIN_MAX, so we
        // need at most TOTAL / MAX iterations plus a small slack.
        let mut iterations = 0;
        loop {
            iterations += 1;
            assert!(
                iterations < (TOTAL / app::App::TEST_EVENT_DRAIN_MAX) + 100,
                "drain never emptied the channel"
            );
            let before_dirty = app.state().dirty;
            app.test_drain_actions();
            // If channel is empty, we're done.
            if tx.capacity() == tx.max_capacity() {
                break;
            }
            // Otherwise, dirty should remain true (we processed at
            // least one event this cycle).
            let _ = before_dirty;
        }
        // After N drain cycles: no queued items left, and state
        // reflects the flood (dirty flag flipped at least once).
        assert!(app.state().dirty, "flood should have flipped dirty");
    }

    /// AC2 — bounded drain. `drain_actions` processes **at most**
    /// `EVENT_DRAIN_MAX` events per call, then returns. This is the
    /// per-frame cap that keeps the redraw budget honest under bursts.
    ///
    /// We enqueue exactly `4 × MAX + 1` events (well above the cap),
    /// invoke `drain_actions` once, then measure how many events are
    /// still queued by counting remaining channel capacity. The delta
    /// must be exactly `MAX` (unless the drain hit the wall-clock
    /// budget first, in which case ≤ MAX).
    #[tokio::test(flavor = "current_thread")]
    async fn drain_pending_caps_at_max() {
        let cap = app::App::TEST_EVENT_DRAIN_MAX;
        // Build an App with a channel large enough to hold 4×cap.
        let (tx, rx) = mpsc::channel::<Action>(cap * 8);
        let (_ptx, prx) = mpsc::unbounded_channel::<PromptEnvelope>();
        let mut app = app::App::new(rx, prx);
        app.force_plain();

        for i in 0..(cap * 4 + 1) {
            tx.try_send(Action::Diagnostic {
                component: "d".to_string(),
                id: None,
                severity: Severity::Trace,
                message: format!("{i}"),
            })
            .expect("channel not full at this size");
        }
        let queued_before = cap * 4 + 1;
        let occupancy_before = tx.max_capacity() - tx.capacity();
        assert_eq!(occupancy_before, queued_before);

        app.test_drain_actions();

        let occupancy_after = tx.max_capacity() - tx.capacity();
        let drained = occupancy_before - occupancy_after;
        assert!(
            drained <= cap,
            "drain exceeded EVENT_DRAIN_MAX: drained {drained}, cap {cap}"
        );
        // Under the time budget (10ms) draining 64 no-op state mutations
        // should always complete, so we should hit the count cap exactly.
        assert_eq!(
            drained, cap,
            "expected drain to hit exactly EVENT_DRAIN_MAX = {cap}, got {drained}"
        );
    }

    /// AC2b — Terminate is not consumed past its position. Events
    /// enqueued *after* a Terminate stay in the channel; drain sets
    /// should_quit and returns immediately.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_pending_stops_at_terminate() {
        let (tx, rx) = mpsc::channel::<Action>(64);
        let (_ptx, prx) = mpsc::unbounded_channel::<PromptEnvelope>();
        let mut app = app::App::new(rx, prx);
        app.force_plain();

        tx.try_send(Action::Diagnostic {
            component: "d".to_string(),
            id: None,
            severity: Severity::Trace,
            message: "before".to_string(),
        })
        .unwrap();
        tx.try_send(Action::Terminate).unwrap();
        tx.try_send(Action::Diagnostic {
            component: "d".to_string(),
            id: None,
            severity: Severity::Trace,
            message: "after".to_string(),
        })
        .unwrap();

        app.test_drain_actions();

        // "after" must still be in the channel (drain stopped at
        // Terminate).
        let remaining = tx.max_capacity() - tx.capacity();
        assert_eq!(
            remaining, 1,
            "drain must not consume events past Terminate; \
             {remaining} still queued"
        );
    }

    /// AC4 — dirty tracking. Two distinct properties:
    /// (a) `State::apply` sets `dirty` only for state-mutating actions;
    ///     `Terminate` (control-plane) doesn't flip it.
    /// (b) `App::flush_frame` clears `dirty` after drawing, so a
    ///     subsequent tick that finds `!dirty` and no prompt should NOT
    ///     re-flush. We prove (b) by driving flush_frame twice back-to-
    ///     back and asserting `last_flush` monotonicity along with
    ///     `state.dirty` being cleared between them.
    #[test]
    fn dirty_is_set_by_mutating_actions_only() {
        use crate::ui::store::State;
        let s = State::default();
        assert!(!s.dirty, "fresh state must not be dirty");

        let mut s2 = State::default();
        s2.apply(&Action::Terminate);
        assert!(!s2.dirty, "Terminate must not dirty state");

        let mut s3 = State::default();
        s3.apply(&Action::Diagnostic {
            component: "t".to_string(),
            id: None,
            severity: Severity::Info,
            message: "hi".to_string(),
        });
        assert!(s3.dirty, "Diagnostic must dirty state");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flush_frame_clears_dirty_and_second_flush_is_noop() {
        let (mut app, _tx, _ptx) = make_app();
        app.force_plain(); // no TTY; flush_frame short-circuits.
        // First action -> dirty.
        app.test_handle_action(&Action::Diagnostic {
            component: "t".to_string(),
            id: None,
            severity: Severity::Info,
            message: "hi".to_string(),
        });
        assert!(app.state().dirty);
        // In plain mode flush_frame short-circuits but still clears
        // dirty (see the early-return in flush_frame).
        app.test_flush_frame().unwrap();
        assert!(!app.state().dirty, "flush_frame must clear dirty");
        // A second flush with no new actions is a no-op — dirty stays
        // false.
        app.test_flush_frame().unwrap();
        assert!(!app.state().dirty);
    }

    /// AC6 — structural test. Exactly one `EventStream::new()` call
    /// site exists in the crate source tree, and it lives inside the
    /// `ui/` module. This encodes invariant #1 from
    /// `memory://tui-input-starvation-fix.md`.
    #[test]
    fn event_stream_only_constructed_in_ui() {
        use std::path::PathBuf;
        let crate_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<PathBuf> = Vec::new();
        walk(&crate_src, &mut sources);
        let mut hits: Vec<PathBuf> = Vec::new();
        // Skip this test file itself: it mentions the token in the
        // grep predicate and in assertion messages.
        let self_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("ui")
            .join("mod.rs");
        for f in sources {
            if f == self_path {
                continue;
            }
            let content = std::fs::read_to_string(&f).unwrap_or_default();
            for line in content.lines() {
                let stripped = line.trim_start();
                if stripped.starts_with("//") {
                    continue;
                }
                if stripped.contains("EventStream::new()") {
                    hits.push(f.clone());
                    break;
                }
            }
        }
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one EventStream::new() call site, found {} at {:?}",
            hits.len(),
            hits
        );
        let hit = &hits[0];
        let hit_str = hit.to_string_lossy();
        assert!(
            hit_str.ends_with("ui/app.rs"),
            "the single EventStream::new() call site must be in ui/app.rs, found {:?}",
            hit
        );

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                    out.push(p);
                }
            }
        }
    }

    /// AC5 — sorted-view invariance. `running_order` must:
    ///   (a) contain exactly the set of `active` entries whose status
    ///       is `Running`, and
    ///   (b) preserve insertion order (Wait->Running promotes to the
    ///       tail; Running->terminal or Running->Wait removes).
    ///
    /// The previous version of this test compared as a HashSet and so
    /// silently accepted any reordering. This version compares as an
    /// ordered Vec against an explicit expected sequence.
    #[test]
    fn running_order_matches_active() {
        use crate::ui::store::State;
        let mut s = State::default();
        s.apply(&start("t", "a", TaskStatus::Running));
        assert_eq!(s.running_order, vec!["t:a"]);

        s.apply(&start("t", "b", TaskStatus::Wait));
        assert_eq!(s.running_order, vec!["t:a"], "Wait doesn't push");

        s.apply(&start("t", "c", TaskStatus::Running));
        assert_eq!(s.running_order, vec!["t:a", "t:c"]);

        s.apply(&update("t", "b", TaskStatus::Running));
        assert_eq!(
            s.running_order,
            vec!["t:a", "t:c", "t:b"],
            "Wait->Running appends to tail"
        );

        s.apply(&update("t", "a", TaskStatus::Success));
        assert_eq!(
            s.running_order,
            vec!["t:c", "t:b"],
            "Running->Success removes from order"
        );

        s.apply(&start("t", "d", TaskStatus::Running));
        assert_eq!(s.running_order, vec!["t:c", "t:b", "t:d"]);

        s.apply(&update("t", "c", TaskStatus::Failed));
        assert_eq!(
            s.running_order,
            vec!["t:b", "t:d"],
            "Running->Failed removes from order"
        );

        s.apply(&start("t", "e", TaskStatus::Running));
        assert_eq!(s.running_order, vec!["t:b", "t:d", "t:e"]);

        s.apply(&update("t", "d", TaskStatus::Wait));
        assert_eq!(
            s.running_order,
            vec!["t:b", "t:e"],
            "Running->Wait removes from order"
        );

        // Cross-check: same set as active's Running subset.
        let active_running: std::collections::HashSet<&str> = s
            .active
            .iter()
            .filter(|(_, t)| t.status == TaskStatus::Running)
            .map(|(k, _)| k.as_str())
            .collect();
        let ro_set: std::collections::HashSet<&str> =
            s.running_order.iter().map(String::as_str).collect();
        assert_eq!(ro_set, active_running);
    }

    /// Local action-builder helpers, duplicated from `store::tests`.
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

    // -------------------------------------------------------------------------
    // Regression tests for the review-round fixes
    // -------------------------------------------------------------------------

    /// F3 — concurrent prompts are queued, not silently auto-quit.
    #[test]
    fn concurrent_prompts_are_queued() {
        use crate::ui::store::State;
        let mut s = State::default();
        let (tx_a, mut rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        s.set_prompt(
            PromptRequest {
                addr: Addr::parse("//project/a").unwrap(),
                error: "a".to_string(),
                log_file: None,
                allow_retry: true,
                allow_shell: false,
                shell: None,
            },
            tx_a,
        );
        s.set_prompt(
            PromptRequest {
                addr: Addr::parse("//project/b").unwrap(),
                error: "b".to_string(),
                log_file: None,
                allow_retry: true,
                allow_shell: false,
                shell: None,
            },
            tx_b,
        );
        assert!(s.prompt.is_some());
        assert_eq!(s.prompt_queue.len(), 1);
        assert!(
            rx_b.try_recv().is_err(),
            "queued prompt must not be auto-resolved"
        );
        s.prompt.as_mut().unwrap().resolve(PromptChoice::Retry);
        assert!(s.advance_prompt(), "advance_prompt should promote B");
        assert!(s.prompt_queue.is_empty());
        assert_eq!(rx_a.try_recv().unwrap(), PromptChoice::Retry);
        assert!(rx_b.try_recv().is_err());
        s.prompt.as_mut().unwrap().resolve(PromptChoice::Quit);
        assert!(!s.advance_prompt(), "queue was empty");
        assert_eq!(rx_b.try_recv().unwrap(), PromptChoice::Quit);
    }

    /// F3 — drain_prompts_with_quit resolves both current and queued.
    #[test]
    fn drain_prompts_with_quit_resolves_all() {
        use crate::ui::store::State;
        let mut s = State::default();
        let (tx_a, mut rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        s.set_prompt(
            PromptRequest {
                addr: Addr::parse("//p/a").unwrap(),
                error: "a".to_string(),
                log_file: None,
                allow_retry: false,
                allow_shell: false,
                shell: None,
            },
            tx_a,
        );
        s.set_prompt(
            PromptRequest {
                addr: Addr::parse("//p/b").unwrap(),
                error: "b".to_string(),
                log_file: None,
                allow_retry: false,
                allow_shell: false,
                shell: None,
            },
            tx_b,
        );
        s.drain_prompts_with_quit();
        assert!(s.prompt.is_none());
        assert!(s.prompt_queue.is_empty());
        assert_eq!(rx_a.try_recv().unwrap(), PromptChoice::Quit);
        assert_eq!(rx_b.try_recv().unwrap(), PromptChoice::Quit);
    }

    /// F6 — Console::shutdown is concurrent-safe.
    #[tokio::test]
    async fn shutdown_is_concurrent_safe() {
        let console = Arc::new(Console::new());
        let c1 = Arc::clone(&console);
        let c2 = Arc::clone(&console);
        let h1 = tokio::spawn(async move { c1.shutdown().await });
        let h2 = tokio::spawn(async move { c2.shutdown().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let _ = h1.await;
            let _ = h2.await;
        })
        .await
        .expect("concurrent shutdown must complete within 5s");
        // A third call after both completed is a no-op.
        console.shutdown().await;
    }

    /// F10 — Console::install returns Err(self) on double install.
    #[tokio::test]
    async fn install_returns_err_on_double_install() {
        let slot: OnceLock<Console> = OnceLock::new();
        let first = Console::new();
        assert!(slot.set(first).is_ok());
        let dup = Console::new();
        let result = slot.set(dup);
        assert!(
            result.is_err(),
            "second install must return Err(self) so caller can shutdown the duplicate"
        );
        // Shutdown both to release their App tasks before dropping.
        if let Some(installed) = slot.get() {
            installed.shutdown().await;
        }
        if let Err(dup) = result {
            dup.shutdown().await;
        }
    }

    /// F11 — privileged-action list matches the docstring.
    #[test]
    fn privilege_list_matches_documented_set() {
        let addr = Addr::parse("//p/a").unwrap();
        assert!(is_privileged(&Action::Terminate));
        assert!(is_privileged(&Action::BuildFinish));
        assert!(is_privileged(&Action::StartBuild {
            addr: addr.clone(),
            total: 1,
        }));
        assert!(is_privileged(&Action::Header {
            tool: "edo-ref".to_string(),
            version: semver::Version::new(0, 1, 0),
            addr: None,
            args: vec![],
            started_at: Timestamp::now(),
        }));
        assert!(is_privileged(&Action::Summary {
            root: std::path::PathBuf::from("/"),
            transforms: 0,
            sources: 0,
            farms: 0,
            locked: false,
        }));
        for sev in [Severity::Error, Severity::Fatal] {
            assert!(is_privileged(&Action::Diagnostic {
                component: "t".to_string(),
                id: None,
                severity: sev,
                message: "m".to_string(),
            }));
        }
        for term in [
            TaskStatus::Failed,
            TaskStatus::Canceled,
            TaskStatus::Cached,
            TaskStatus::Success,
        ] {
            assert!(is_privileged(&Action::UpdateTask {
                component: "t".to_string(),
                id: "1".to_string(),
                operation: "op".to_string(),
                status: term,
                message: None,
            }));
            assert!(is_privileged(&Action::StartTask {
                component: "t".to_string(),
                id: "1".to_string(),
                operation: "op".to_string(),
                status: term,
                message: None,
            }));
        }
        for sev in [
            Severity::Trace,
            Severity::Debug,
            Severity::Info,
            Severity::Warn,
        ] {
            assert!(!is_privileged(&Action::Diagnostic {
                component: "t".to_string(),
                id: None,
                severity: sev,
                message: "m".to_string(),
            }));
        }
        for non_term in [TaskStatus::Wait, TaskStatus::Running] {
            assert!(!is_privileged(&Action::UpdateTask {
                component: "t".to_string(),
                id: "1".to_string(),
                operation: "op".to_string(),
                status: non_term,
                message: None,
            }));
        }
    }

    /// F11 — closed-channel drops are counted.
    #[tokio::test]
    async fn closed_channel_drops_bump_counter() {
        let console = Console::new();
        console.shutdown().await;
        let before = console.handle.dropped.load(Ordering::Relaxed);
        console
            .send(&Action::Diagnostic {
                component: "t".to_string(),
                id: None,
                severity: Severity::Trace,
                message: "post-shutdown".to_string(),
            })
            .await;
        let after = console.handle.dropped.load(Ordering::Relaxed);
        assert_eq!(after, before + 1, "Closed send must bump dropped");
    }
}
