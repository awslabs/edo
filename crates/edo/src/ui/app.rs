//! The single async task that owns the terminal and drives the UI.
//!
//! Design (see plan `driving-poetic-spaniel.md`):
//!
//! - Exactly one async task, not two. `crossterm::event::EventStream` lives
//!   on the App stack and is polled directly by the loop's `select!`. There
//!   is no separate "driver" task and no `Event` mpsc.
//! - Two `tokio::sync::mpsc` channels: `action_rx` (bounded, capacity
//!   `CHANNEL_CAPACITY`) and `prompt_rx` (unbounded, prompts are rare and
//!   must not be dropped). Both benefit from tokio's cooperative-yield
//!   scheduling — the earlier flume-based version had to hand-roll a
//!   bounded drain to avoid starving sibling arms (see
//!   `memory://tui-render-overflow-freeze.md`).
//! - Frame-budgeted rendering: no branch calls `terminal.draw` directly.
//!   Actions mutate `state`, push scroll-back into an in-memory buffer,
//!   and set `state.dirty`. A `tokio::time::interval` at `FRAME_HZ`
//!   flushes the scroll-back with **one** `insert_before` and **one**
//!   `terminal.draw` per frame. Ticks that find `!dirty` and no running
//!   tasks skip the redraw entirely.

use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{Event as CrossTermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, is_raw_mode_enabled},
    tty::IsTty,
};
use futures::StreamExt;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Widget},
};
use snafu::ResultExt;
use tokio::sync::mpsc;

use super::{
    PromptChoice, PromptEnvelope, Result,
    action::Action,
    error,
    render::{draw_frame, next_selection},
    store::{self, PromptSelect, State},
    ui::Ui,
};

// -----------------------------------------------------------------------------
// Loop-level constants
// -----------------------------------------------------------------------------

/// Redraw cadence when the state is dirty.
const FRAME_HZ: u32 = 30;

/// Slower cadence for the "only running-task timers changed" case. Keeps
/// the elapsed-time counter alive without spending 30 FPS on it.
const STATUS_HZ: u32 = 4;

/// Height of the inline viewport (worker rows + 1 statusline). Matches
/// the default `[scheduler] workers = 8` pool so all worker rows fit
/// on-screen without the "(+N more)" overflow marker being a permanent
/// fixture.
pub(super) const VIEWPORT_HEIGHT: u16 = 9;

/// Maximum number of actions consumed inside a single frame flush. Same
/// value as the previous flume-based implementation
/// (`memory://tui-render-overflow-freeze.md`): protects the frame budget
/// from being blown by a huge action burst while still amortising the
/// insert_before / draw cost.
const EVENT_DRAIN_MAX: usize = 64;

/// Wallclock budget for a single frame's action drain. Belt-and-braces
/// on top of `EVENT_DRAIN_MAX`: if an action handler ever becomes
/// expensive we still cut off at 10ms and yield.
const EVENT_DRAIN_BUDGET: Duration = Duration::from_millis(10);

// -----------------------------------------------------------------------------
// Mode
// -----------------------------------------------------------------------------

/// Interactive vs plain output selection. Picked at construction time
/// based on whether stderr is a TTY.
#[allow(clippy::large_enum_variant)]
pub(super) enum Mode {
    Interactive {
        driver: Ui,
        terminal: Terminal<CrosstermBackend<std::io::Stderr>>,
    },
    Plain {
        sink: std::io::Stderr,
    },
}

/// Auto-detect interactive vs plain mode based on stderr being a TTY.
pub(super) fn build_mode() -> Mode {
    let stderr = std::io::stderr();
    if !stderr.is_tty() {
        return Mode::Plain {
            sink: std::io::stderr(),
        };
    }
    // Anchor the inline viewport at the top of the terminal. Without
    // this the viewport lands at whatever row the shell left the cursor
    // on, which visually floats mid-screen until enough `insert_before`
    // rows push it down. Clearing + `MoveTo(0, 0)` makes the viewport
    // start at the top; `insert_before` then grows the scroll-back
    // downward from row 0 until it fills the screen, at which point the
    // terminal emulator's native scroll-back takes over.
    {
        use std::io::Write;
        let mut out = std::io::stderr();
        let _ = execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0),);
        let _ = out.flush();
    }

    // Interactive: build both the ratatui inline viewport and the Ui
    // RAII guard. If either fails we degrade to plain mode.
    let terminal_res = Terminal::with_options(
        CrosstermBackend::new(std::io::stderr()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT.max(3)),
        },
    );
    let driver_res = Ui::new();
    let (Ok(terminal), Ok(driver)) = (terminal_res, driver_res) else {
        return Mode::Plain {
            sink: std::io::stderr(),
        };
    };
    // Enable raw mode on entry to the loop, not here — `run_interactive`
    // calls `driver.enter()` explicitly so the raw-mode toggle happens
    // inside the App task's runtime, matching the EventStream poll.
    let _ = enable_raw_mode();
    Mode::Interactive { driver, terminal }
}

// -----------------------------------------------------------------------------
// App
// -----------------------------------------------------------------------------

/// The App task. Owns:
/// - the ratatui `Terminal` (via `Mode::Interactive`),
/// - the `Ui` RAII guard for raw-mode,
/// - the two mpsc receivers (`action_rx`, `prompt_rx`),
/// - the `State` (single-owned, no cross-task sharing),
/// - a scroll-back staging buffer (`pending_scrollback`) that
///   accumulates `insert_before` rows and is flushed once per frame.
pub struct App {
    state: State,
    should_quit: bool,
    action_rx: mpsc::Receiver<Action>,
    prompt_rx: mpsc::UnboundedReceiver<PromptEnvelope>,
    mode: Mode,
    /// Scroll-back rows staged for the next frame flush. Accumulated by
    /// `handle_action` and drained by `flush_frame` via a single
    /// `terminal.insert_before` call.
    pending_scrollback: Vec<Line<'static>>,
    /// Cached terminal width, refreshed on Resize events and before
    /// each frame flush. Ensures every scroll-back row staged in a
    /// single drain uses the same width — a mid-drain resize won't
    /// split the batch across two widths.
    cached_width: u16,
    /// Whether the crossterm `EventStream` has ended (`None`) or
    /// repeatedly errored. Once set, the events arm is skipped so a
    /// terminated stream doesn't busy-loop the biased select.
    events_done: bool,
    /// Wall-clock of the last `flush_frame`. Used by
    /// `maybe_flush_under_load` to guarantee a minimum redraw cadence
    /// even when the frame ticker is starved by the biased action arm.
    last_flush: Instant,
}

impl App {
    pub fn new(
        action_rx: mpsc::Receiver<Action>,
        prompt_rx: mpsc::UnboundedReceiver<PromptEnvelope>,
    ) -> Self {
        let mode = build_mode();
        Self {
            state: State::default(),
            should_quit: false,
            action_rx,
            prompt_rx,
            mode,
            pending_scrollback: Vec::new(),
            // Optimistic default; refreshed inside `run_interactive` on
            // first flush and on every Resize event.
            cached_width: u16::MAX,
            events_done: false,
            last_flush: Instant::now(),
        }
    }

    /// Run the App loop until a `Terminate` action arrives (or every
    /// sender drops, which is equivalent). Restores the terminal on
    /// every exit path.
    pub async fn run(&mut self) -> Result<()> {
        let result = match &mut self.mode {
            Mode::Interactive { .. } => self.run_interactive().await,
            Mode::Plain { .. } => self.run_plain().await,
        };
        let restore = self.restore_terminal();
        result.and(restore)
    }

    // -------------------------------------------------------------------------
    // Interactive loop
    // -------------------------------------------------------------------------

    async fn run_interactive(&mut self) -> Result<()> {
        // Enter raw mode inside the task so the raw-mode toggle and the
        // EventStream construction happen on the same runtime worker.
        if let Mode::Interactive { driver, .. } = &mut self.mode {
            driver.enter()?;
        }

        // Own the EventStream directly. Invariant: this is the only
        // `EventStream::new()` call site in the crate — enforced by the
        // structural test `event_stream_only_constructed_in_ui`.
        let mut events = EventStream::new();

        // Single frame ticker at FRAME_HZ. Fires 30x/second; each tick
        // decides whether to redraw based on:
        // - `state.dirty`: an action or key modified visible state.
        // - `state.prompt.is_some()`: a prompt overlay wants
        //   selection-highlight feedback on every tick.
        // - "running-timer freshness": if any task is Running, redraw
        //   at least every `1000 / STATUS_HZ` ms so the elapsed-time
        //   counter advances. We track this via `last_flush` rather
        //   than a second `tokio::time::Interval`.
        //
        // `MissedTickBehavior::Skip` so ticks that fall behind
        // wall-clock don't fire back-to-back (see the memory note on
        // the previous tick regressions).
        let mut frame = tokio::time::interval(Duration::from_millis(1000 / u64::from(FRAME_HZ)));
        frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let status_period = Duration::from_millis(1000 / u64::from(STATUS_HZ));

        // Draw once so the viewport is visible before any event arrives.
        self.state.dirty = true;
        self.flush_frame()?;

        loop {
            tokio::select! {
                biased;
                // Crossterm key/mouse/resize events. Directly polled —
                // no intermediate channel. Gated on `!events_done` so
                // that a terminated stream (Some(None) forever) does
                // NOT busy-loop this arm: once `events.next()` returns
                // None or an error we mark the stream dead and skip
                // this arm on subsequent iterations, giving the other
                // arms a chance.
                maybe_event = events.next(), if !self.events_done => {
                    match maybe_event {
                        Some(Ok(evt)) => self.handle_crossterm_event(evt)?,
                        Some(Err(e)) => {
                            tracing::warn!(
                                subsystem = "ui",
                                error = %e,
                                "event stream error"
                            );
                        }
                        None => {
                            tracing::info!(
                                subsystem = "ui",
                                "event stream ended (tty closed)"
                            );
                            self.events_done = true;
                        }
                    }
                }
                // Action channel. Wake, drain up to EVENT_DRAIN_MAX
                // events (or EVENT_DRAIN_BUDGET wallclock), then let the
                // frame tick redraw. This is the batching pattern
                // documented in `memory://tui-render-overflow-freeze.md`
                // §"Bounded drain in the receiver arm".
                maybe_action = self.action_rx.recv() => {
                    match maybe_action {
                        Some(Action::Terminate) => break,
                        Some(action) => {
                            self.handle_action(&action);
                            self.drain_actions();
                            // Under sustained action-side load, biased
                            // select would keep this arm perpetually
                            // ready and starve the frame / prompt arms.
                            // Two mitigations after every drain:
                            //   1. Pick up any queued prompts so a
                            //      concurrent transform failure is
                            //      surfaced promptly.
                            //   2. If dirty and enough wall-clock has
                            //      elapsed since the last flush, redraw
                            //      here so the user sees progress even
                            //      when the frame tick is starved.
                            self.drain_prompts_ready();
                            self.maybe_flush_under_load()?;
                        }
                        None => break,
                    }
                }
                maybe_prompt = self.prompt_rx.recv() => {
                    if let Some((request, responder)) = maybe_prompt {
                        self.state.set_prompt(request, responder);
                    }
                }
                _ = frame.tick() => {
                    // Frame tick: the *only* branch that calls
                    // terminal.draw. Redraw when:
                    //   - state is dirty, or
                    //   - a prompt is showing (selection highlight),
                    //     or
                    //   - a running task exists AND STATUS_HZ has
                    //     elapsed since the last flush (elapsed-timer
                    //     freshness — see file-level docstring).
                    let running_timer_due = !self.state.running_order.is_empty()
                        && self.last_flush.elapsed() >= status_period;
                    if self.state.dirty
                        || self.state.prompt.is_some()
                        || running_timer_due
                    {
                        self.flush_frame()?;
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }

        // Final flush so the last state is visible before teardown.
        let _ = self.flush_frame();

        // Any still-visible or still-queued prompt must be resolved so
        // a scheduler task never hangs on a dropped sender.
        self.state.drain_prompts_with_quit();
        // Drain any prompts still on the wire.
        while let Ok((_, tx)) = self.prompt_rx.try_recv() {
            let _ = tx.send(PromptChoice::Quit);
        }
        Ok(())
    }

    /// Drain up to `EVENT_DRAIN_MAX` additional actions (or until
    /// `EVENT_DRAIN_BUDGET` elapses) into state, without awaiting.
    /// Called after the first action wakeup so a burst is amortised
    /// into one frame instead of one draw per action.
    ///
    /// Signals `should_quit` when a `Terminate` is observed mid-drain.
    /// Events queued *after* the Terminate stay in the channel (they'll
    /// be dropped when receivers close), matching the contract in the
    /// memory note: "flush and exit".
    fn drain_actions(&mut self) {
        let start = Instant::now();
        for _ in 0..EVENT_DRAIN_MAX {
            if start.elapsed() >= EVENT_DRAIN_BUDGET {
                break;
            }
            match self.action_rx.try_recv() {
                Ok(Action::Terminate) => {
                    self.should_quit = true;
                    return;
                }
                Ok(action) => self.handle_action(&action),
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.should_quit = true;
                    return;
                }
            }
        }
    }

    /// Non-blocking sweep of the prompt channel. Under sustained
    /// action-side load `biased` select would keep the action arm
    /// perpetually ready and starve `prompt_rx.recv()`. Calling this
    /// after every drain guarantees a queued prompt is surfaced within
    /// one drain-batch of arrival.
    fn drain_prompts_ready(&mut self) {
        while let Ok((request, responder)) = self.prompt_rx.try_recv() {
            self.state.set_prompt(request, responder);
        }
    }

    /// Force a `flush_frame` when the frame ticker has been starved for
    /// more than one frame period. Guarantees the user sees progress
    /// even when the action arm is perpetually ready — combined with
    /// the two-arm biased select this preserves the "prompt priority
    /// > actions" ordering while still bounding redraw latency.
    fn maybe_flush_under_load(&mut self) -> Result<()> {
        // Frame period at FRAME_HZ. Match the redraw ceiling used by
        // the frame ticker itself so bursts don't cause excessive
        // redraws.
        let frame_period = Duration::from_millis(1000 / u64::from(FRAME_HZ));
        if (self.state.dirty || self.state.prompt.is_some())
            && self.last_flush.elapsed() >= frame_period
        {
            self.flush_frame()?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Plain mode
    // -------------------------------------------------------------------------

    async fn run_plain(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                biased;
                maybe_action = self.action_rx.recv() => {
                    match maybe_action {
                        Some(Action::Terminate) => break,
                        Some(action) => {
                            self.state.apply(&action);
                            let Mode::Plain { sink } = &mut self.mode else {
                                unreachable!("run_plain called with non-plain mode");
                            };
                            for line in action.to_lines_width(u16::MAX) {
                                let s: String = line
                                    .spans
                                    .iter()
                                    .map(|sp| sp.content.as_ref())
                                    .collect::<Vec<_>>()
                                    .join("");
                                use std::io::Write;
                                let _ = writeln!(sink, "{s}");
                            }
                            use std::io::Write;
                            let _ = sink.flush();
                        }
                        None => break,
                    }
                }
                maybe_prompt = self.prompt_rx.recv() => {
                    // Plain mode has no interactive input; auto-quit so
                    // the scheduler unblocks.
                    if let Some((_, responder)) = maybe_prompt {
                        let _ = responder.send(PromptChoice::Quit);
                    }
                }
            }
        }
        // Drain any queued prompts on shutdown.
        while let Ok((_, tx)) = self.prompt_rx.try_recv() {
            let _ = tx.send(PromptChoice::Quit);
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Action / event handling
    // -------------------------------------------------------------------------

    /// Fold an action into state and stage its scroll-back rows. Does
    /// **not** call `terminal.draw` — that is the frame tick's job.
    ///
    /// Uses `self.cached_width` so an entire drain batch renders at
    /// one consistent width even if a resize fires mid-drain.
    fn handle_action(&mut self, action: &Action) {
        self.state.apply(action);
        self.state.dirty = true;
        if matches!(self.mode, Mode::Interactive { .. }) {
            for line in action.to_lines_width(self.cached_width) {
                self.pending_scrollback.push(to_owned_line(line));
            }
        }
    }

    /// Flush staged scroll-back and redraw the viewport. Called exactly
    /// once per frame tick (when dirty) — see the invariants at the top
    /// of the file.
    ///
    /// Handles two edge cases that were previously silent bugs:
    /// - `insert_before` returning `Err`: log the error and re-stage
    ///   the scroll-back so the next frame retries. The alternative
    ///   (silent `let _ = ...`) drops user-visible log lines on any
    ///   transient I/O error.
    /// - `pending_scrollback.len() > u16::MAX`: chunk into u16-sized
    ///   batches so no line is silently clipped.
    fn flush_frame(&mut self) -> Result<()> {
        let Mode::Interactive { terminal, .. } = &mut self.mode else {
            self.state.dirty = false;
            return Ok(());
        };
        // Refresh cached width so subsequent drains use the current
        // terminal size; costs one ioctl per frame, not per action.
        self.cached_width = terminal.size().map(|r| r.width).unwrap_or(u16::MAX);
        if !self.pending_scrollback.is_empty() {
            let mut lines = std::mem::take(&mut self.pending_scrollback);
            // Peel off chunks of at most u16::MAX rows so the height
            // cast in `insert_before` never truncates.
            while !lines.is_empty() {
                let take = lines.len().min(u16::MAX as usize);
                let batch: Vec<Line<'static>> = lines.drain(..take).collect();
                let height = batch.len() as u16;
                if let Err(e) = terminal.insert_before(height, |buf: &mut Buffer| {
                    let area: Rect = buf.area;
                    let p = Paragraph::new(batch.clone());
                    p.render(area, buf);
                }) {
                    tracing::warn!(
                        subsystem = "ui",
                        error = %e,
                        rows = batch.len(),
                        "insert_before failed; re-staging scroll-back for next frame"
                    );
                    // Put the unwritten batch and remaining lines back
                    // at the head of the queue so we retry them next
                    // frame. Order is preserved.
                    let mut restaged = batch;
                    restaged.extend(lines);
                    self.pending_scrollback = restaged;
                    break;
                }
            }
        }
        let state = &self.state;
        terminal
            .draw(|frame| draw_frame(state, frame))
            .context(error::IoSnafu)?;
        self.state.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }

    fn handle_crossterm_event(&mut self, event: CrossTermEvent) -> Result<()> {
        match event {
            CrossTermEvent::Key(key) => self.handle_key(key),
            CrossTermEvent::Resize(cols, _rows) => {
                // A resize invalidates the viewport layout and the
                // cached width. Refresh both, then force a redraw.
                self.cached_width = cols;
                self.state.dirty = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn handle_key(&mut self, key_event: KeyEvent) -> Result<()> {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }
        // Ctrl+C exits.
        if key_event.code == KeyCode::Char('c')
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.should_quit = true;
            // Drain the entire prompt queue with Quit — Ctrl+C is a
            // "get me out" signal, not a per-prompt choice.
            self.state.drain_prompts_with_quit();
            return Ok(());
        }

        // Prompt navigation / commit.
        if self.state.prompt.is_some() {
            self.handle_prompt_key(key_event.code)?;
            // Any prompt key mutates selection or dismisses the prompt:
            // schedule a redraw so the selection highlight moves.
            self.state.dirty = true;
        }
        Ok(())
    }

    fn handle_prompt_key(&mut self, code: KeyCode) -> Result<()> {
        // Arrow / Tab navigation for visual selection first.
        {
            let Some(prompt) = self.state.prompt.as_mut() else {
                return Ok(());
            };
            match code {
                KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                    prompt.selected = next_selection(prompt, prompt.selected, true);
                    return Ok(());
                }
                KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                    prompt.selected = next_selection(prompt, prompt.selected, false);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Terminal choices.
        match code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.resolve_if(|p| p.allow_retry, PromptChoice::Retry)
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.resolve_current(PromptChoice::Quit);
                Ok(())
            }
            KeyCode::Char('s') | KeyCode::Char('S') => self.handle_shell(),
            KeyCode::Char('v') | KeyCode::Char('V') => self.handle_view_log(),
            KeyCode::Enter => {
                // Commit the currently-selected option.
                let selected = self
                    .state
                    .prompt
                    .as_ref()
                    .map(|p| p.selected)
                    .unwrap_or(PromptSelect::Quit);
                match selected {
                    PromptSelect::ViewLog => self.handle_view_log(),
                    PromptSelect::Retry => self.resolve_if(|p| p.allow_retry, PromptChoice::Retry),
                    PromptSelect::Shell => self.handle_shell(),
                    PromptSelect::Quit => {
                        self.resolve_current(PromptChoice::Quit);
                        Ok(())
                    }
                }
            }
            _ => Ok(()),
        }
    }

    fn resolve_if<F>(&mut self, cond: F, choice: PromptChoice) -> Result<()>
    where
        F: FnOnce(&store::Prompt) -> bool,
    {
        let allowed = self.state.prompt.as_ref().map(cond).unwrap_or(false);
        if allowed {
            self.resolve_current(choice);
        }
        Ok(())
    }

    /// Resolve the currently-visible prompt with `choice` and promote
    /// the next queued prompt (if any) into the visible slot. Central
    /// choke-point for every terminal decision so we never leave a
    /// queued prompt orphaned.
    fn resolve_current(&mut self, choice: PromptChoice) {
        if let Some(mut p) = self.state.prompt.take() {
            p.resolve(choice);
        }
        // `advance_prompt` returns false when the queue is empty —
        // that's the normal case and needs no special handling.
        let _ = self.state.advance_prompt();
        // Either resolving or advancing changed what's on screen.
        self.state.dirty = true;
    }

    fn handle_shell(&mut self) -> Result<()> {
        let allow_shell = self
            .state
            .prompt
            .as_ref()
            .map(|p| p.allow_shell)
            .unwrap_or(false);
        if !allow_shell {
            return Ok(());
        }
        self.suspend_and_run(|prompt| {
            if let Some(cb) = prompt.shell.as_mut() {
                // Shell callback runs a child process that blocks until
                // the user exits. `block_in_place` tells tokio to move
                // other tasks off this worker so a shell session doesn't
                // starve N-1 workers on a multi-thread runtime. Matches
                // `handle_view_log` below.
                tokio::task::block_in_place(cb)
            } else {
                Ok(())
            }
        })
    }

    fn handle_view_log(&mut self) -> Result<()> {
        let has_log = self
            .state
            .prompt
            .as_ref()
            .and_then(|p| p.log_file.as_ref())
            .is_some();
        if !has_log {
            return Ok(());
        }
        self.suspend_and_run(|prompt| {
            let Some(path) = prompt.log_file.as_ref() else {
                return Ok(());
            };
            let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
            // The pager blocks until the user quits it. Wrap in
            // `block_in_place` so the runtime can migrate other tasks
            // off this worker while we wait.
            let status = tokio::task::block_in_place(|| {
                std::process::Command::new(pager).arg(path).status()
            });
            match status {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        })
    }

    /// Suspend the terminal, invoke `f` on the active prompt, then
    /// re-enable raw mode and issue a `terminal.clear()`. The clear is
    /// intentional: a pager/shell may have left arbitrary output
    /// on-screen and only a clear-then-redraw restores the viewport
    /// cleanly.
    fn suspend_and_run<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&mut store::Prompt) -> std::io::Result<()>,
    {
        let Mode::Interactive { driver, terminal } = &mut self.mode else {
            return Ok(());
        };
        driver.suspend()?;
        use std::io::Write;
        let mut out = std::io::stderr();
        let _ = execute!(out, Clear(ClearType::FromCursorDown), cursor::Show);
        let _ = writeln!(out);
        let _ = out.flush();
        if let Some(prompt) = self.state.prompt.as_mut()
            && let Err(e) = f(prompt)
        {
            tracing::warn!(subsystem = "ui", "prompt sub-invocation failed: {e}");
        }
        driver.resume()?;
        // Force a full repaint after resuming.
        let _ = terminal.clear();
        self.state.dirty = true;
        Ok(())
    }

    fn restore_terminal(&self) -> Result<()> {
        if matches!(self.mode, Mode::Interactive { .. }) {
            use std::io::Write;
            if is_raw_mode_enabled().unwrap_or(false) {
                let _ = disable_raw_mode();
            }
            let mut out = std::io::stderr();
            let _ = execute!(out, Clear(ClearType::FromCursorDown), cursor::Show);
            let _ = writeln!(out);
            let _ = out.flush();
        }
        Ok(())
    }
}

/// Detach a `Line<'_>` from any borrowed spans so it can outlive the
/// action that produced it. The staged scroll-back buffer holds these
/// across frame ticks, so we need an owned copy.
fn to_owned_line(line: Line<'_>) -> Line<'static> {
    use ratatui::text::Span;
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    let mut owned = Line::from(spans);
    owned.style = line.style;
    owned.alignment = line.alignment;
    owned
}

// -----------------------------------------------------------------------------
// Test helpers (only compiled under #[cfg(test)])
// -----------------------------------------------------------------------------

#[cfg(test)]
impl App {
    pub(super) fn is_plain(&self) -> bool {
        matches!(self.mode, Mode::Plain { .. })
    }

    /// Test-only accessor: read State directly. Used by AC5 to inspect
    /// `running_order` after driving a scripted action sequence.
    pub(super) fn state(&self) -> &State {
        &self.state
    }

    /// Test-only mutator: force the App into `Mode::Plain` regardless
    /// of TTY detection. Lets tests exercise the plain-mode loop when
    /// running under a TTY (rare but possible).
    #[allow(dead_code)]
    pub(super) fn force_plain(&mut self) {
        self.mode = Mode::Plain {
            sink: std::io::stderr(),
        };
    }

    /// Test-only: drive `handle_action` + `drain_actions` synchronously
    /// so unit tests can exercise the interactive path (state
    /// transitions, dirty flag, running_order maintenance,
    /// scroll-back accumulation, drain cap) without spinning up
    /// crossterm or a real TTY.
    pub(super) fn test_handle_action(&mut self, action: &Action) {
        self.handle_action(action);
    }

    /// Test-only: expose `drain_actions` so AC2 can verify the cap
    /// after synthetically enqueuing 4×MAX actions.
    pub(super) fn test_drain_actions(&mut self) {
        self.drain_actions();
    }

    /// Test-only: expose `flush_frame` so AC4 can prove
    /// no-dirty-no-draw at the loop level.
    pub(super) fn test_flush_frame(&mut self) -> Result<()> {
        self.flush_frame()
    }

    /// Test-only: read the drain-cap constant so tests don't hard-code
    /// duplicate literals.
    pub(super) const TEST_EVENT_DRAIN_MAX: usize = EVENT_DRAIN_MAX;
}
