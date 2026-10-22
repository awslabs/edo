//! Log manager and tracing initialization.
//!
//! [`LogManager`] owns the log directory and initializes the process-wide
//! `tracing` subscriber. [`LogVerbosity`] controls the tracing filter
//! level.
//!
//! # Three channels
//!
//! A single `tracing_subscriber::registry()` fans events out to:
//!
//! - **Task progress bars (stderr)** — [`TaskLayer`] intercepts every
//!   event that carries both an `id` and a `status` field (i.e. the
//!   output of [`ui_start_task!`](crate::ui_start_task) /
//!   [`ui_update_task!`](crate::ui_update_task)) and drives one
//!   [`indicatif::ProgressBar`] per task `id` on a shared
//!   [`indicatif::MultiProgress`]. Terminal statuses (`success`,
//!   `failed`, `cached`, `canceled`) finish the bar with a durable
//!   summary line; running-phase updates only swap the bar's message,
//!   and `waiting` retires any open bar without printing.
//! - **Compact console formatter (stderr)** — [`ConsoleFormatter`]
//!   renders every *other* `info!` / `warn!` / `error!` etc. as
//!   `[HH:MM:SS]  LEVEL  message  key=val key=val`. Field noise
//!   (`subsystem`, `component`, `op`) is suppressed at INFO, shown
//!   at DEBUG+. Writes go through [`MultiProgress::println`] so bars
//!   don't clobber prints, and are deferred entirely while an
//!   interactive prompt owns the terminal (see [`LogManager::prompt`]).
//! - **Structured log (file)** — JSON Lines to a configurable path
//!   (default `<logdir>/edo.jsonl`, disabled via `--event-log none`).
//!   Emits *every* event unchanged, including the task lifecycle
//!   ones the console suppresses, so postmortem analysis has the full
//!   trace. Call [`LogManager::shutdown`] before the process exits so
//!   the tail of the file is flushed.
//!
//! All three layers share the same [`Targets`] filter, so
//! `--debug` / `--trace` affects each consistently.
//!
//! # Canonical structured-field schema
//!
//! Every `info!` / `debug!` / `trace!` / `warn!` / `error!` call — and
//! every expansion of the [`ui_info!`](crate::ui_info) family of
//! macros — uses a fixed vocabulary of keyword fields:
//!
//! | field        | type | when                          |
//! |--------------|------|-------------------------------|
//! | `subsystem`  | str  | always                        |
//! | `component`  | str  | when subsystem has variants   |
//! | `addr`       | str  | when an `Addr` is in scope    |
//! | `id`         | str  | task lifecycle events         |
//! | `op`         | str  | for state-change lines        |
//! | `status`     | str  | task lifecycle events         |
//! | `phase`      | str  | task lifecycle events, optional |
//!
//! The presence of both `id` and `status` on an event is the signal
//! [`TaskLayer`] uses to route it to a progress bar instead of a
//! printed line.

use std::{
    collections::{HashMap, hash_map::Entry},
    convert::Infallible,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use jiff::Zoned;
use owo_colors::{OwoColorize, Stream};
use parking_lot::{Mutex, MutexGuard};
use snafu::ResultExt;
use tokio::{
    fs::{create_dir_all, remove_dir_all},
    sync::OnceCell,
};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    level_filters::LevelFilter,
};
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    Layer,
    filter::Targets,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    layer::{Context as LayerContext, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

pub use super::Log;
use super::{ContextResult as Result, error};
use crate::log::{TaskStatus, format_duration};

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogVerbosity {
    /// Emit trace-level and above.
    Trace,
    /// Emit debug-level and above.
    Debug,
    /// Emit info-level and above (default).
    Info,
}

/// Where — if anywhere — the JSONL structured log gets written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventLog {
    /// Write to `<logdir>/edo.jsonl`.
    #[default]
    Default,
    /// Write to an explicit path.
    Path(PathBuf),
    /// Do not emit the JSONL layer at all.
    Disabled,
}

impl EventLog {
    /// Resolve the setting to a concrete path (or `None` when disabled),
    /// given a logdir to use for `Default`.
    fn resolve(&self, logdir: &Path) -> Option<PathBuf> {
        match self {
            Self::Default => Some(logdir.join("edo.jsonl")),
            Self::Path(p) => Some(p.clone()),
            Self::Disabled => None,
        }
    }
}

/// Parses the `--event-log` CLI value: `none` disables the log, `default`
/// selects [`EventLog::Default`] (both case-insensitive), and anything
/// else is taken as a path.
impl FromStr for EventLog {
    type Err = Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(if s.eq_ignore_ascii_case("none") {
            Self::Disabled
        } else if s.eq_ignore_ascii_case("default") {
            Self::Default
        } else {
            Self::Path(PathBuf::from(s))
        })
    }
}

/// Process-wide handle to the shared [`MultiProgress`] and task registry.
///
/// Cloned by [`TaskLayer`] into every worker thread that emits an event;
/// [`LogManager::prompt`] uses it to hand the terminal to an interactive
/// prompt.
#[derive(Clone)]
struct ProgressState {
    multi: Arc<MultiProgress>,
    tasks: Arc<Mutex<HashMap<String, TaskEntry>>>,
    /// True when stderr is a TTY. Controls whether we draw spinner rows
    /// or fall back to plain "terminal-state only" one-liners.
    interactive: bool,
    gate: Arc<PromptGate>,
}

impl ProgressState {
    fn new(multi: Arc<MultiProgress>, interactive: bool) -> Self {
        Self {
            multi,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            interactive,
            gate: Arc::new(PromptGate::default()),
        }
    }
}

/// Terminal ownership hand-off between the worker threads and an
/// interactive prompt.
///
/// While `active` is set, an interactive prompt owns stderr. Writers
/// must not print — they push their line onto `pending` and it is
/// replayed once the prompt releases the terminal. This keeps the
/// remaining transforms running at full speed: they never block on the
/// progress lock and never scribble over the prompt.
#[derive(Default)]
struct PromptGate {
    active: AtomicBool,
    pending: Mutex<Vec<String>>,
}

impl PromptGate {
    /// Defer `line` when a prompt owns the terminal. Returns `true` when
    /// the line was captured and must not be printed.
    fn defer(&self, line: &str) -> bool {
        if !self.active.load(AtomicOrdering::Acquire) {
            return false;
        }
        let mut pending = self.pending.lock();
        // Re-check under the lock: `release` clears the flag while holding
        // it, so a line pushed after that would never be printed.
        if !self.active.load(AtomicOrdering::Acquire) {
            return false;
        }
        pending.push(line.to_string());
        true
    }

    /// Replays deferred lines through `emit` in order, then reopens the
    /// terminal to direct writes.
    ///
    /// The flag stays set until `pending` is observed empty under its
    /// lock, so a line logged during the replay is queued behind the
    /// older ones instead of jumping ahead of them.
    fn release(&self, mut emit: impl FnMut(&str)) {
        loop {
            let batch = {
                let mut pending = self.pending.lock();
                if pending.is_empty() {
                    self.active.store(false, AtomicOrdering::Release);
                    return;
                }
                std::mem::take(&mut *pending)
            };
            for line in &batch {
                emit(line);
            }
        }
    }
}

struct TaskEntry {
    bar: ProgressBar,
    started: Instant,
}

/// Manages the log directory and tracing subscriber for a build session.
#[derive(Clone)]
pub struct LogManager {
    inner: Arc<Inner>,
}

/// Process-wide singleton: the global `tracing` subscriber can only be
/// installed once, so every `init` after the first returns this manager.
static LOG_MANAGER: OnceCell<LogManager> = OnceCell::const_new();

impl LogManager {
    /// Initializes the log directory at `path` and sets up the tracing subscriber.
    ///
    /// The tracing subscriber is process-global and installed by the first
    /// call; concurrent first calls wait for it. Subsequent calls return a
    /// clone of that `LogManager` and keep its `path`, `verbosity`, and
    /// `event_log`, logging a warning when the requested ones differ. This
    /// lets a binary construct multiple `Context`s without tripping
    /// `SetGlobalDefaultError`.
    pub async fn init<P: AsRef<Path>>(
        path: P,
        verbosity: LogVerbosity,
        event_log: EventLog,
    ) -> Result<Self> {
        let path = path.as_ref();
        let manager = LOG_MANAGER
            .get_or_try_init(|| async {
                Ok::<_, error::ContextError>(Self {
                    inner: Arc::new(Inner::init(path, verbosity, event_log.clone()).await?),
                })
            })
            .await?;
        manager
            .inner
            .warn_if_reconfigured(path, verbosity, &event_log);
        Ok(manager.clone())
    }

    /// Flushes and closes the JSONL event log, if one is open.
    ///
    /// The process-wide manager lives in a `static` and is never dropped,
    /// so binaries must call this on every exit path or the most recent
    /// events are lost. Events emitted afterwards still reach the console
    /// but are no longer written to the event log.
    pub fn shutdown() {
        let Some(manager) = LOG_MANAGER.get() else {
            return;
        };
        let guard = manager.inner.appender_guard.lock().take();
        drop(guard);
    }

    /// Creates a new [`Log`] file for the given task `id`.
    pub async fn create(&self, id: &str) -> Result<Log> {
        self.inner.create(self, id).await
    }

    /// Runs `f` as the sole owner of the terminal, for an interactive
    /// prompt (e.g. `dialoguer::Select`).
    ///
    /// Unlike [`MultiProgress::suspend`], this does *not* hold the
    /// progress lock for the duration: the bar rows are cleared and the
    /// draw target is swapped to hidden, so sibling transforms keep
    /// running and logging without blocking on the console. Their output
    /// is buffered by the prompt gate and replayed when `f` returns or
    /// panics.
    ///
    /// Concurrent prompts are serialised on the console lock, so only
    /// one `Select` ever reads the keyboard at a time.
    pub fn prompt<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _session = PromptSession::begin(&self.inner);
        f()
    }

    /// Removes and recreates the log directory.
    pub async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

struct Inner {
    path: PathBuf,
    verbosity: LogVerbosity,
    event_log: EventLog,
    lock: Mutex<()>,
    progress: ProgressState,
    /// Keeps the non-blocking JSONL writer alive; taking and dropping it
    /// (see [`LogManager::shutdown`]) flushes pending lines. `None` when
    /// the JSONL sink is disabled or already shut down.
    appender_guard: Mutex<Option<WorkerGuard>>,
}

impl Inner {
    async fn init(logdir: &Path, verbosity: LogVerbosity, event_log: EventLog) -> Result<Self> {
        if logdir.exists() {
            // If the logdir already exists we want to clean it up, it should only be used for a single run
            remove_dir_all(&logdir).await.context(error::IoSnafu)?;
        }
        create_dir_all(&logdir).await.context(error::IoSnafu)?;

        // Bars only make sense when stderr is a TTY. Under a pipe /
        // redirect we install a hidden draw target so `ProgressBar`
        // updates are no-ops; terminal-state lines still fall through
        // to a raw stderr write in [`write_line`].
        let interactive = std::io::stderr().is_terminal();
        let multi = MultiProgress::with_draw_target(draw_target(interactive));
        let progress = ProgressState::new(Arc::new(multi), interactive);

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

        let show_meta = !matches!(verbosity, LogVerbosity::Info);

        // Console fmt layer. Its writer routes through `print_line`, so
        // printed lines never tear across a bar redraw and never land on
        // top of an active prompt.
        let fmt_writer = ProgressWriter {
            progress: progress.clone(),
        };
        let fmt_layer = tracing_subscriber::fmt::layer()
            .event_format(ConsoleFormatter { show_meta })
            .fmt_fields(ConsoleFieldFormatter)
            .with_writer(fmt_writer)
            .with_filter(filter.clone())
            .with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
                !is_task_event(meta)
            }));

        // Task lifecycle events drive progress bars only; they are never
        // printed as log lines.
        let task_layer = TaskLayer {
            progress: progress.clone(),
        }
        .with_filter(filter.clone());

        // Optional JSON layer: one JSON object per line, span list + all
        // structured event fields preserved. Sees *every* event —
        // including the task ones — so the JSONL is a full trace.
        let (json_layer, appender_guard) = if let Some(json_path) = event_log.resolve(logdir) {
            if let Some(parent) = json_path.parent()
                && !parent.exists()
            {
                create_dir_all(parent).await.context(error::IoSnafu)?;
            }
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&json_path)
                .context(error::IoSnafu)?;
            // Block instead of dropping lines when the writer falls behind
            // (e.g. under `--trace`): the event log must be complete.
            let (json_writer, guard) = NonBlockingBuilder::default().lossy(false).finish(log_file);
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(json_writer)
                .with_filter(filter.clone());
            (Some(layer), Some(guard))
        } else {
            (None, None)
        };

        tracing_subscriber::registry()
            .with(json_layer)
            .with(fmt_layer)
            .with(task_layer)
            .try_init()
            .context(error::LogSnafu)?;
        Ok(Self {
            path: logdir.to_path_buf(),
            verbosity,
            event_log,
            lock: Mutex::new(()),
            progress,
            appender_guard: Mutex::new(appender_guard),
        })
    }

    /// Warns when a later `init` asks for settings the singleton ignores.
    fn warn_if_reconfigured(&self, path: &Path, verbosity: LogVerbosity, event_log: &EventLog) {
        if self.path == path && self.verbosity == verbosity && self.event_log == *event_log {
            return;
        }
        warn!(
            subsystem = "context",
            component = "log",
            op = "init",
            active_path = ?self.path,
            requested_path = ?path,
            active_verbosity = ?self.verbosity,
            requested_verbosity = ?verbosity,
            active_event_log = ?self.event_log,
            requested_event_log = ?event_log,
            "logging is already initialized; keeping the active settings"
        );
    }

    async fn clear(&self) -> Result<()> {
        remove_dir_all(&self.path).await.context(error::IoSnafu)?;
        create_dir_all(&self.path).await.context(error::IoSnafu)?;
        Ok(())
    }

    async fn create(&self, root: &LogManager, id: &str) -> Result<Log> {
        let file_name = format!("{id}.log");
        let file_target = self.path.join(file_name.clone());
        Log::new(root, &file_target)
    }
}

/// Exclusive terminal ownership for one interactive prompt.
///
/// Restores the bar rows and replays deferred lines on drop, so the
/// terminal is handed back even when the prompt panics.
struct PromptSession<'a> {
    progress: &'a ProgressState,
    _console: MutexGuard<'a, ()>,
}

impl<'a> PromptSession<'a> {
    fn begin(inner: &'a Inner) -> Self {
        let console = inner.lock.lock();
        let progress = &inner.progress;
        progress.gate.active.store(true, AtomicOrdering::Release);
        // Erase the spinner rows, then stop drawing entirely: a hidden
        // draw target turns every sibling `set_message` / `println` into
        // a no-op that returns immediately instead of queueing behind a
        // lock we would otherwise hold for the whole prompt.
        let _ = progress.multi.clear();
        progress.multi.set_draw_target(ProgressDrawTarget::hidden());
        Self {
            progress,
            _console: console,
        }
    }
}

impl Drop for PromptSession<'_> {
    fn drop(&mut self) {
        let progress = self.progress;
        progress
            .multi
            .set_draw_target(draw_target(progress.interactive));
        progress.gate.release(|line| write_line(progress, line));
    }
}

/// The draw target matching the current stderr: live bars on a TTY,
/// hidden under a pipe or redirect.
fn draw_target(interactive: bool) -> ProgressDrawTarget {
    if interactive {
        ProgressDrawTarget::stderr_with_hz(12)
    } else {
        ProgressDrawTarget::hidden()
    }
}

/// Print one durable line above the bar rows, deferring it while an
/// interactive prompt owns the terminal.
fn print_line(progress: &ProgressState, line: &str) {
    if progress.gate.defer(line) {
        return;
    }
    write_line(progress, line);
}

/// Write one line to stderr above the bar rows, bypassing the prompt gate.
///
/// `MultiProgress::println` is the natural choice on a live target, but
/// it drops output when the target is hidden (e.g. stderr is a pipe),
/// and these lines must always land somewhere.
fn write_line(progress: &ProgressState, line: &str) {
    if progress.interactive {
        let _ = progress.multi.println(line);
    } else {
        progress.multi.suspend(|| {
            let _ = writeln!(std::io::stderr(), "{line}");
        });
    }
}

/// True when the event's static callsite declares both `id` and
/// `status`. This is the marker for `ui_start_task!` / `ui_update_task!`
/// callsites and is used to route them to [`TaskLayer`] while excluding
/// them from the console fmt layer.
fn is_task_event(meta: &tracing::Metadata<'_>) -> bool {
    let fields = meta.fields();
    fields.field("id").is_some() && fields.field("status").is_some()
}

// ---------------------------------------------------------------------
// Task -> progress-bar layer
// ---------------------------------------------------------------------

/// `tracing_subscriber::Layer` that projects task lifecycle events onto
/// per-`id` [`ProgressBar`]s.
struct TaskLayer {
    progress: ProgressState,
}

/// Terminal work decided under the task-map lock and performed after it
/// is released.
enum BarUpdate {
    /// Nothing to draw.
    None,
    /// A running task went back to waiting: clear its bar silently.
    Retire(ProgressBar),
    /// The task reached a terminal status; `None` when it never had a bar.
    Finish(Option<TaskEntry>),
    /// Swap the message on an existing bar.
    Refresh(ProgressBar),
    /// Style and start a freshly registered bar.
    Open(ProgressBar),
}

impl<S> Layer<S> for TaskLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _cx: LayerContext<'_, S>) {
        if !is_task_event(event.metadata()) {
            return;
        }
        let mut v = TaskFieldVisitor::default();
        event.record(&mut v);
        let (Some(id), Some(status)) = (v.id, v.status) else {
            return;
        };
        let Ok(status) = status.parse::<TaskStatus>() else {
            return;
        };
        let phase = v.phase.as_deref().unwrap_or("");

        let update = {
            let mut tasks = self.progress.tasks.lock();
            match (tasks.entry(id.clone()), status) {
                (Entry::Occupied(occ), TaskStatus::Wait) => BarUpdate::Retire(occ.remove().bar),
                (Entry::Vacant(_), TaskStatus::Wait) => BarUpdate::None,
                (Entry::Occupied(occ), s) if s.is_terminal() => {
                    BarUpdate::Finish(Some(occ.remove()))
                }
                (Entry::Vacant(_), s) if s.is_terminal() => BarUpdate::Finish(None),
                (Entry::Occupied(occ), _) => BarUpdate::Refresh(occ.get().bar.clone()),
                (Entry::Vacant(vac), _) => {
                    let bar = self.progress.multi.add(ProgressBar::new_spinner());
                    vac.insert(TaskEntry {
                        bar: bar.clone(),
                        started: Instant::now(),
                    });
                    BarUpdate::Open(bar)
                }
            }
        };

        match update {
            BarUpdate::None => {}
            BarUpdate::Retire(bar) => self.retire(&bar),
            BarUpdate::Finish(Some(TaskEntry { bar, started })) => {
                // Retire the bar before printing to avoid a one-frame redraw.
                self.retire(&bar);
                self.print_terminal(&id, status, phase, started.elapsed());
            }
            BarUpdate::Finish(None) => self.print_terminal(&id, status, phase, Duration::ZERO),
            BarUpdate::Refresh(bar) => {
                bar.set_message(render_running_message(&id, status, phase));
                bar.tick();
            }
            BarUpdate::Open(bar) => {
                bar.set_style(running_style());
                bar.set_message(render_running_message(&id, status, phase));
                bar.enable_steady_tick(Duration::from_millis(120));
            }
        }
    }
}

impl TaskLayer {
    /// Remove a bar's spinner row from the layout.
    fn retire(&self, bar: &ProgressBar) {
        bar.finish_and_clear();
        self.progress.multi.remove(bar);
    }

    /// Emit a durable one-line summary for a terminal task status,
    /// above the remaining bar rows.
    fn print_terminal(&self, id: &str, status: TaskStatus, phase: &str, elapsed: Duration) {
        let line = render_terminal_line(id, status, phase, elapsed);
        print_line(&self.progress, &line);
    }
}

fn running_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.green} {msg}")
        .expect("static spinner template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"])
}

/// Format the message shown next to the spinner for a running task.
fn render_running_message(id: &str, status: TaskStatus, phase: &str) -> String {
    let addr = id
        .if_supports_color(Stream::Stderr, |t| t.bold().to_string())
        .to_string();
    let status_col = colorize_status(status);
    if phase.is_empty() {
        format!("{addr}  {status_col}")
    } else {
        let phase_col = phase
            .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
            .to_string();
        format!("{addr}  {status_col}  {phase_col}")
    }
}

/// Render the durable one-line summary emitted for a terminal task
/// status.  `elapsed == ZERO` renders without the duration slot (used
/// for cache-hits and tasks that skipped straight past `running`).
fn render_terminal_line(id: &str, status: TaskStatus, phase: &str, elapsed: Duration) -> String {
    let timestamp = Zoned::now().strftime("%H:%M:%S").to_string();
    let ts = format!("[{timestamp}]")
        .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
        .to_string();
    let symbol = terminal_symbol(status);
    let addr = id
        .if_supports_color(Stream::Stderr, |t| t.bold().to_string())
        .to_string();
    let status_col = colorize_status(status);
    let dur = if elapsed == Duration::ZERO {
        String::new()
    } else {
        format!(
            "  {}",
            format_duration(elapsed).if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
        )
    };
    let phase_part = if phase.is_empty() {
        String::new()
    } else {
        format!(
            "  {}",
            phase.if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
        )
    };
    format!("{ts} {symbol}  {addr}  {status_col}{phase_part}{dur}")
}

fn terminal_symbol(status: TaskStatus) -> String {
    match status {
        TaskStatus::Wait | TaskStatus::Running => "•".to_string(),
        TaskStatus::Success => "✔"
            .if_supports_color(Stream::Stderr, |t| t.bold().green().to_string())
            .to_string(),
        TaskStatus::Cached => "◇"
            .if_supports_color(Stream::Stderr, |t| t.bold().cyan().to_string())
            .to_string(),
        TaskStatus::Failed => "✖"
            .if_supports_color(Stream::Stderr, |t| t.bold().red().to_string())
            .to_string(),
        TaskStatus::Canceled => "⊘"
            .if_supports_color(Stream::Stderr, |t| t.bold().yellow().to_string())
            .to_string(),
    }
}

fn colorize_status(status: TaskStatus) -> String {
    let text = status.as_str();
    match status {
        TaskStatus::Running => text
            .if_supports_color(Stream::Stderr, |t| t.bright_blue().to_string())
            .to_string(),
        TaskStatus::Wait => text
            .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
            .to_string(),
        TaskStatus::Success => text
            .if_supports_color(Stream::Stderr, |t| t.green().to_string())
            .to_string(),
        TaskStatus::Cached => text
            .if_supports_color(Stream::Stderr, |t| t.cyan().to_string())
            .to_string(),
        TaskStatus::Failed => text
            .if_supports_color(Stream::Stderr, |t| t.red().to_string())
            .to_string(),
        TaskStatus::Canceled => text
            .if_supports_color(Stream::Stderr, |t| t.yellow().to_string())
            .to_string(),
    }
}

/// Collects the task fields, which the `ui_*_task!` macros record as `&str`.
#[derive(Default)]
struct TaskFieldVisitor {
    id: Option<String>,
    status: Option<String>,
    phase: Option<String>,
}

impl Visit for TaskFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "id" => self.id = Some(value.to_string()),
            "status" => self.status = Some(value.to_string()),
            "phase" => self.phase = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

// ---------------------------------------------------------------------
// Compact console formatter for non-task events
// ---------------------------------------------------------------------

/// Event formatter for non-task events on the console layer.
///
/// Emits `[HH:MM:SS] LEVEL  message  key=val key=val`.  When an `addr`
/// field is present it is bolded and hoisted to the front of the
/// message. Meta fields (`subsystem`, `component`, `op`) are suppressed
/// at INFO level; they reappear at DEBUG/TRACE where the extra context
/// is worth the noise.
#[derive(Clone, Copy)]
struct ConsoleFormatter {
    show_meta: bool,
}

impl<S, N> FormatEvent<S, N> for ConsoleFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let level = *meta.level();

        let mut v = LineVisitor::default();
        event.record(&mut v);

        let timestamp = Zoned::now().strftime("%H:%M:%S").to_string();
        let level_str = match level {
            Level::ERROR => "ERROR"
                .if_supports_color(Stream::Stderr, |t| t.bold().red().to_string())
                .to_string(),
            Level::WARN => "WARN "
                .if_supports_color(Stream::Stderr, |t| t.bold().yellow().to_string())
                .to_string(),
            Level::INFO => "INFO "
                .if_supports_color(Stream::Stderr, |t| t.bold().green().to_string())
                .to_string(),
            Level::DEBUG => "DEBUG"
                .if_supports_color(Stream::Stderr, |t| t.bold().blue().to_string())
                .to_string(),
            Level::TRACE => "TRACE"
                .if_supports_color(Stream::Stderr, |t| t.bold().cyan().to_string())
                .to_string(),
        };
        let ts = format!("[{timestamp}]")
            .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
            .to_string();

        write!(writer, "{ts} {level_str}  ")?;

        if let Some(addr) = v.addr.as_deref() {
            let addr = addr
                .if_supports_color(Stream::Stderr, |t| t.bold().to_string())
                .to_string();
            write!(writer, "{addr}  ")?;
        }

        if let Some(msg) = v.message.as_deref() {
            write!(writer, "{msg}")?;
        }

        let mut needs_separator = v.message.is_some();
        for (k, val) in &v.extras {
            if !self.show_meta && matches!(k.as_str(), "subsystem" | "component" | "op") {
                continue;
            }
            if needs_separator {
                write!(writer, "  ")?;
            }
            needs_separator = true;
            let key = k
                .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
                .to_string();
            write!(writer, "{key}={val}")?;
        }

        writeln!(writer)?;
        Ok(())
    }
}

/// Field formatter used by the console fmt layer for span-attached
/// fields.  Non-task spans don't render into the console line, so we
/// simply drop them; JSON sink handles the structured payload.
#[derive(Clone, Copy)]
struct ConsoleFieldFormatter;

impl<'a> FormatFields<'a> for ConsoleFieldFormatter {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(
        &self,
        _writer: Writer<'a>,
        _fields: R,
    ) -> std::fmt::Result {
        Ok(())
    }
}

#[derive(Default)]
struct LineVisitor {
    message: Option<String>,
    addr: Option<String>,
    extras: Vec<(String, String)>,
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_string()),
            "addr" => self.addr = Some(value.to_string()),
            other => self.extras.push((other.to_string(), value.to_string())),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        match field.name() {
            "message" => self.message = Some(rendered),
            "addr" => self.addr = Some(rendered),
            other => self.extras.push((other.to_string(), rendered)),
        }
    }
}

// ---------------------------------------------------------------------
// MultiProgress-aware writer for the fmt layer
// ---------------------------------------------------------------------

/// A `MakeWriter` that funnels every write through [`print_line`] so a
/// printed line never tears a mid-render bar frame, and never lands on
/// top of an active prompt.
#[derive(Clone)]
struct ProgressWriter {
    progress: ProgressState,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ProgressWriter {
    type Writer = ProgressWriterHandle;

    fn make_writer(&'a self) -> Self::Writer {
        ProgressWriterHandle {
            progress: self.progress.clone(),
            buf: Vec::new(),
        }
    }
}

struct ProgressWriterHandle {
    progress: ProgressState,
    buf: Vec<u8>,
}

impl std::io::Write for ProgressWriterHandle {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.buf);
        let line = String::from_utf8_lossy(&line);
        // Trailing newline is re-added by the printers below.
        print_line(&self.progress, line.trim_end_matches('\n'));
        Ok(())
    }
}

impl Drop for ProgressWriterHandle {
    fn drop(&mut self) {
        let _ = <Self as std::io::Write>::flush(self);
    }
}

/// Shared test support: provides a process-wide singleton `LogManager` so that
/// `tracing_subscriber::try_init` is only called once per test binary, regardless
/// of how many test modules need a `LogManager`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{EventLog, LogManager, LogVerbosity};
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
            let mgr = LogManager::init(&logs_dir, LogVerbosity::Info, EventLog::Disabled)
                .await
                .expect("LogManager::init");
            *guard = Some((mgr, dir));
        }
        guard.as_ref().unwrap().0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_verbosity_eq() {
        assert_eq!(LogVerbosity::Info, LogVerbosity::Info);
        assert_eq!(LogVerbosity::Debug, LogVerbosity::Debug);
        assert_eq!(LogVerbosity::Trace, LogVerbosity::Trace);
        assert_ne!(LogVerbosity::Info, LogVerbosity::Debug);
        assert_ne!(LogVerbosity::Debug, LogVerbosity::Trace);
        assert_ne!(LogVerbosity::Info, LogVerbosity::Trace);
    }

    #[test]
    fn test_event_log_from_str_keywords_are_case_insensitive() {
        for (input, expected) in [
            ("none", EventLog::Disabled),
            ("NONE", EventLog::Disabled),
            ("default", EventLog::Default),
            ("Default", EventLog::Default),
            ("./Defaults", EventLog::Path(PathBuf::from("./Defaults"))),
            (
                "/tmp/edo.jsonl",
                EventLog::Path(PathBuf::from("/tmp/edo.jsonl")),
            ),
        ] {
            assert_eq!(
                input.parse::<EventLog>().unwrap(),
                expected,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn test_event_log_resolve_places_default_in_logdir() {
        let logdir = Path::new("/work/.edo/logs");
        assert_eq!(
            EventLog::Default.resolve(logdir),
            Some(logdir.join("edo.jsonl"))
        );
        assert_eq!(
            EventLog::Path(PathBuf::from("/elsewhere/x.jsonl")).resolve(logdir),
            Some(PathBuf::from("/elsewhere/x.jsonl"))
        );
        assert_eq!(EventLog::Disabled.resolve(logdir), None);
    }

    /// Records whether each event it sees is classified as a task event.
    struct RoutingProbe(Arc<Mutex<Vec<bool>>>);

    impl<S: Subscriber> Layer<S> for RoutingProbe {
        fn on_event(&self, event: &Event<'_>, _cx: LayerContext<'_, S>) {
            self.0.lock().push(is_task_event(event.metadata()));
        }
    }

    #[test]
    fn test_is_task_event_matches_only_task_lifecycle_macros() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(RoutingProbe(seen.clone()));

        tracing::subscriber::with_default(subscriber, || {
            crate::ui_start_task!(subsystem = "test", op = "probe"; "//a", TaskStatus::Wait, None);
            crate::ui_update_task!(
                subsystem = "test",
                op = "probe";
                "//a",
                TaskStatus::Running,
                Some("stage".into())
            );
            crate::ui_info!(subsystem = "test", id = "//a"; "id without status");
            tracing::info!(status = "running", "status without id");
        });

        assert_eq!(*seen.lock(), [true, true, false, false]);
    }

    #[test]
    fn test_render_terminal_line_includes_elapsed_only_when_nonzero() {
        let timed = render_terminal_line(
            "//a",
            TaskStatus::Success,
            "execute",
            Duration::from_millis(1_500),
        );
        let untimed = render_terminal_line("//a", TaskStatus::Cached, "cache-hit", Duration::ZERO);

        for (line, status, phase) in [
            (&timed, "success", "execute"),
            (&untimed, "cached", "cache-hit"),
        ] {
            assert!(line.contains("//a"), "{line:?}");
            assert!(line.contains(status), "{line:?}");
            assert!(line.contains(phase), "{line:?}");
        }
        assert!(timed.contains("1.5s"), "{timed:?}");
        assert!(!untimed.contains("0ms"), "{untimed:?}");
    }

    /// A line logged while a prompt owns the terminal must be buffered,
    /// not printed on top of the prompt, and must be replayed once the
    /// prompt returns.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn prompt_defers_console_lines_until_it_returns() {
        let mgr = test_support::shared_log_manager().await;
        let deferred = mgr.prompt(|| {
            tracing::info!(subsystem = "test", "emitted while the prompt is up");
            mgr.inner.progress.gate.pending.lock().len()
        });
        assert_eq!(deferred, 1, "the line must be buffered, not printed");
        assert!(
            mgr.inner.progress.gate.pending.lock().is_empty(),
            "buffered lines must be drained when the prompt returns"
        );
    }

    /// A panicking prompt must still hand the terminal back.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn test_prompt_panic_releases_terminal() {
        let mgr = test_support::shared_log_manager().await;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mgr.prompt(|| {
                tracing::info!(subsystem = "test", "emitted before the prompt panics");
                panic!("prompt failed");
            })
        }));

        assert!(outcome.is_err(), "the panic must propagate to the caller");
        let gate = &mgr.inner.progress.gate;
        assert!(
            !gate.active.load(AtomicOrdering::Acquire),
            "the gate must reopen after a panic"
        );
        assert!(
            gate.pending.lock().is_empty(),
            "lines buffered before the panic must be replayed"
        );
    }

    #[test]
    fn test_prompt_gate_release_replays_in_order_then_reopens() {
        let gate = PromptGate::default();
        gate.active.store(true, AtomicOrdering::Release);
        assert!(gate.defer("first"));
        assert!(gate.defer("second"));

        let mut replayed = Vec::new();
        gate.release(|line| {
            replayed.push(line.to_string());
            // Lines logged during the replay queue behind older ones.
            if line == "first" {
                assert!(gate.defer("third"));
            }
        });

        assert_eq!(replayed, ["first", "second", "third"]);
        assert!(!gate.defer("after"), "the gate must be open after release");
    }

    /// Outside a prompt the gate is transparent: nothing is buffered.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn lines_are_not_deferred_without_an_active_prompt() {
        let mgr = test_support::shared_log_manager().await;
        tracing::info!(subsystem = "test", "emitted with no prompt up");
        assert!(mgr.inner.progress.gate.pending.lock().is_empty());
    }

    /// Smoke-test that `shared_log_manager` returns a usable `LogManager` and
    /// that calling it multiple times yields the same underlying instance
    /// (i.e. `create` works on both).
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn shared_log_manager_returns_usable_manager() {
        let mgr = test_support::shared_log_manager().await;
        // Creating a log file must not panic or error.
        let _log = mgr.create("logmgr-smoke").await.expect("create log");
    }
}
