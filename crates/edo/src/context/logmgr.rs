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
//!   summary line; running-phase updates only swap the bar's message.
//!   No log line is printed for those updates — that was the source
//!   of the scrolling "task updated" spam.
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
//!   trace.
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
//!
//! The presence of both `id` and `status` on an event is the signal
//! [`TaskLayer`] uses to route it to a progress bar instead of a
//! printed line.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use jiff::Zoned;
use owo_colors::{OwoColorize, Stream};
use parking_lot::Mutex;
use snafu::ResultExt;
use std::{
    collections::HashMap,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};
use tokio::fs::{create_dir_all, remove_dir_all};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    level_filters::LevelFilter,
};
use tracing_appender::non_blocking::WorkerGuard;
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
#[derive(PartialEq, Eq, Debug)]
pub enum LogVerbosity {
    /// Emit trace-level and above.
    Trace,
    /// Emit debug-level and above.
    Debug,
    /// Emit info-level and above (default).
    Info,
}

/// Where — if anywhere — the JSONL structured log gets written.
#[derive(Clone, Debug, Default)]
pub enum EventLog {
    /// Write to `<logdir>/edo.jsonl` (the historical default).
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
        // Re-check under the lock: a prompt that finished between the
        // load and the lock has already drained `pending`, so a line
        // pushed now would never be printed.
        if !self.active.load(AtomicOrdering::Acquire) {
            return false;
        }
        pending.push(line.to_string());
        true
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

/// Process-wide singleton. The `tracing` global subscriber can only be
/// installed once per process (`try_init` fails on the second call), and
/// callers like `twoliter publish kit` legitimately construct multiple
/// `Context`s in a single run (one per target arch). We cache the first
/// `LogManager` and hand back clones on subsequent `init` calls so both
/// the subscriber and the log directory survive.
static LOG_MANAGER: OnceLock<LogManager> = OnceLock::new();

impl LogManager {
    /// Initializes the log directory at `path` and sets up the tracing subscriber.
    ///
    /// The tracing subscriber is process-global and installed on the first
    /// call. Subsequent calls in the same process return a clone of the
    /// first `LogManager` — the `path`, `verbosity`, and `event_log`
    /// arguments are ignored after the first successful init. This lets a
    /// binary construct multiple `Context`s without tripping
    /// `SetGlobalDefaultError`.
    pub async fn init<P: AsRef<Path>>(
        path: P,
        verbosity: LogVerbosity,
        event_log: EventLog,
    ) -> Result<Self> {
        if let Some(existing) = LOG_MANAGER.get() {
            return Ok(existing.clone());
        }
        let mgr = Self {
            inner: Arc::new(Inner::init(path, verbosity, event_log).await?),
        };
        // Race between two concurrent `init` calls: the loser drops its
        // freshly built manager and returns the winner's. Both paths
        // installed the same global subscriber via `try_init`, but only
        // the winner's `try_init` call succeeded — the loser's Inner
        // would have already returned `Err(SetGlobalDefault)` and we
        // wouldn't reach here. So in practice this is only a defensive
        // guard against the first-call fast path racing itself.
        Ok(LOG_MANAGER.get_or_init(|| mgr).clone())
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
    /// is buffered by the prompt gate and replayed when `f` returns.
    ///
    /// Concurrent prompts are serialised on the console lock, so only
    /// one `Select` ever reads the keyboard at a time.
    pub fn prompt<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.inner.prompt(f)
    }

    /// Removes and recreates the log directory.
    pub async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

struct Inner {
    path: PathBuf,
    lock: Mutex<()>,
    progress: ProgressState,
    /// Keeps the non-blocking tracing-appender worker alive for the
    /// lifetime of the [`LogManager`]. Dropping it flushes any pending
    /// log lines. `None` when the JSONL sink is disabled.
    _appender_guard: Option<WorkerGuard>,
}

impl Inner {
    pub async fn init<P: AsRef<Path>>(
        path: P,
        verbosity: LogVerbosity,
        event_log: EventLog,
    ) -> Result<Self> {
        let logdir = path.as_ref();
        if logdir.exists() {
            // If the logdir already exists we want to clean it up, it should only be used for a single run
            remove_dir_all(&logdir).await.context(error::IoSnafu)?;
        }
        create_dir_all(&logdir).await.context(error::IoSnafu)?;

        // Bars only make sense when stderr is a TTY. Under a pipe /
        // redirect we install a hidden draw target so `ProgressBar`
        // updates are no-ops; terminal-state lines still fall through
        // to a raw stderr write in [`print_line`].
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

        // Task-driven progress-bar layer. Consumes task lifecycle events
        // (those with both `id` and `status`) and never forwards them
        // to a printed line — that would just be the spam we replaced.
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
            let (json_writer, guard) = tracing_appender::non_blocking(log_file);
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
            lock: Mutex::new(()),
            progress,
            _appender_guard: appender_guard,
        })
    }

    pub async fn clear(&self) -> Result<()> {
        remove_dir_all(&self.path).await.context(error::IoSnafu)?;
        create_dir_all(&self.path).await.context(error::IoSnafu)?;
        Ok(())
    }

    pub async fn create(&self, root: &LogManager, id: &str) -> Result<Log> {
        let file_name = format!("{id}.log");
        let file_target = self.path.join(file_name.clone());
        Log::new(root, &file_target)
    }

    fn prompt<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _console = self.lock.lock();
        let multi = &self.progress.multi;
        self.progress
            .gate
            .active
            .store(true, AtomicOrdering::Release);
        // Erase the spinner rows, then stop drawing entirely: a hidden
        // draw target turns every sibling `set_message` / `println` into
        // a no-op that returns immediately instead of queueing behind a
        // lock we would otherwise hold for the whole prompt.
        let _ = multi.clear();
        multi.set_draw_target(ProgressDrawTarget::hidden());

        let ret = f();

        multi.set_draw_target(draw_target(self.progress.interactive));
        // Clear the flag before draining so a writer that raced us
        // prints for itself rather than pushing onto a drained buffer.
        self.progress
            .gate
            .active
            .store(false, AtomicOrdering::Release);
        let deferred = std::mem::take(&mut *self.progress.gate.pending.lock());
        for line in deferred {
            print_line(&self.progress, &line);
        }
        ret
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
///
/// `MultiProgress::println` is the natural choice on a live target, but
/// it drops output when the target is hidden (e.g. stderr is a pipe),
/// and these lines must always land somewhere.
fn print_line(progress: &ProgressState, line: &str) {
    if progress.gate.defer(line) {
        return;
    }
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

        // Three status buckets:
        //   - "waiting": never render — the task hasn't started yet, so
        //     an idle spinner is just visual noise.  JSONL still gets it.
        //   - "running": open (or update the message on) a spinner row.
        //   - anything else (success / cached / failed / canceled):
        //     terminal.  Remove the bar (if any) and print a durable
        //     one-line summary above the remaining bars via
        //     `MultiProgress::println`, so it lands in scrollback.
        let phase = v.phase.as_deref().unwrap_or("");

        if status == "waiting" {
            return;
        }

        let is_terminal = matches!(
            status.as_str(),
            "success" | "failed" | "cached" | "canceled"
        );

        let mut tasks = self.progress.tasks.lock();
        let entry = tasks.entry(id.clone());

        match entry {
            std::collections::hash_map::Entry::Occupied(mut occ) => {
                if is_terminal {
                    let TaskEntry { bar, started } = occ.remove();
                    let elapsed = started.elapsed();
                    // Retire the bar first (removes the spinner row from
                    // the MultiProgress layout), then print the durable
                    // summary. The order matters — otherwise `println`
                    // would redraw the just-finished bar under the
                    // printed line for one frame.
                    bar.finish_and_clear();
                    self.progress.multi.remove(&bar);
                    self.print_terminal(&id, &status, phase, elapsed);
                } else {
                    let e = occ.get_mut();
                    e.bar
                        .set_message(render_running_message(&id, &status, phase));
                    e.bar.tick();
                }
            }
            std::collections::hash_map::Entry::Vacant(vac) => {
                if is_terminal {
                    // Task went terminal without ever having been in a
                    // running state (e.g. cache-hit at fetch): no bar
                    // to retire, just print the durable line.
                    self.print_terminal(&id, &status, phase, Duration::ZERO);
                } else {
                    let bar = self.progress.multi.add(ProgressBar::new_spinner());
                    bar.set_style(running_style());
                    bar.enable_steady_tick(Duration::from_millis(120));
                    bar.set_message(render_running_message(&id, &status, phase));
                    vac.insert(TaskEntry {
                        bar,
                        started: Instant::now(),
                    });
                }
            }
        }
    }
}

impl TaskLayer {
    /// Emit a durable one-line summary for a terminal task status,
    /// above the remaining bar rows.
    fn print_terminal(&self, id: &str, status: &str, phase: &str, elapsed: Duration) {
        let line = render_terminal_line(id, status, phase, elapsed);
        print_line(&self.progress, &line);
    }
}

fn running_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.green} {msg}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"])
}

/// Format the message shown next to the spinner for a running task.
fn render_running_message(id: &str, status: &str, phase: &str) -> String {
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
fn render_terminal_line(id: &str, status: &str, phase: &str, elapsed: Duration) -> String {
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
            format_std_duration(elapsed)
                .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
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

fn terminal_symbol(status: &str) -> String {
    match status {
        "success" => "✔"
            .if_supports_color(Stream::Stderr, |t| t.bold().green().to_string())
            .to_string(),
        "cached" => "◇"
            .if_supports_color(Stream::Stderr, |t| t.bold().cyan().to_string())
            .to_string(),
        "failed" => "✖"
            .if_supports_color(Stream::Stderr, |t| t.bold().red().to_string())
            .to_string(),
        "canceled" => "⊘"
            .if_supports_color(Stream::Stderr, |t| t.bold().yellow().to_string())
            .to_string(),
        _ => "•".to_string(),
    }
}

fn colorize_status(status: &str) -> String {
    match status {
        "running" => status
            .if_supports_color(Stream::Stderr, |t| t.bright_blue().to_string())
            .to_string(),
        "waiting" => status
            .if_supports_color(Stream::Stderr, |t| t.dimmed().to_string())
            .to_string(),
        "success" => status
            .if_supports_color(Stream::Stderr, |t| t.green().to_string())
            .to_string(),
        "cached" => status
            .if_supports_color(Stream::Stderr, |t| t.cyan().to_string())
            .to_string(),
        "failed" => status
            .if_supports_color(Stream::Stderr, |t| t.red().to_string())
            .to_string(),
        "canceled" => status
            .if_supports_color(Stream::Stderr, |t| t.yellow().to_string())
            .to_string(),
        _ => status.to_string(),
    }
}

fn format_std_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs_f = ms as f64 / 1000.0;
    if secs_f < 60.0 {
        return format!("{secs_f:.1}s");
    }
    let total_secs = (ms / 1000) as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    format!("{hours}h{rem_mins}m")
}

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

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `ui_update_task!` records `phase = ?Option<String>`, which
        // arrives as `Some("stage")` / `None`. Strip that wrapping so we
        // just see `stage` in the bar text.
        let raw = format!("{:?}", value);
        let cleaned = strip_option_debug(&raw);
        match field.name() {
            "id" => {
                if self.id.is_none() {
                    self.id = Some(cleaned);
                }
            }
            "status" => {
                if self.status.is_none() {
                    self.status = Some(cleaned);
                }
            }
            "phase" if self.phase.is_none() && !cleaned.is_empty() => {
                self.phase = Some(cleaned);
            }
            _ => {}
        }
    }
}

/// Turn `Some("foo")` -> `foo`, `None` -> `""`, and strip surrounding
/// double-quotes from a plain `Debug` string.
fn strip_option_debug(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed == "None" {
        return String::new();
    }
    let inner = if let Some(rest) = trimmed.strip_prefix("Some(") {
        rest.strip_suffix(')').unwrap_or(rest)
    } else {
        trimmed
    };
    let inner = inner.trim();
    let unquoted = inner
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(inner);
    unquoted.to_string()
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

        // Skip meta fields at INFO to keep the line free of the
        // `subsystem=/component=/op=` chorus. The JSONL sink still gets
        // everything.
        let show_meta = self.show_meta;
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

        let mut first = v.message.is_some();
        for (k, val) in &v.extras {
            if !show_meta && matches!(k.as_str(), "subsystem" | "component" | "op") {
                continue;
            }
            if !first {
                write!(writer, "  ")?;
                first = true;
            } else {
                write!(writer, "  ")?;
            }
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
        let raw = format!("{:?}", value);
        // `%$id` records as a plain string; `?value` for other fields
        // records with Debug's surrounding quotes / Some(..) noise.
        // Normalize both paths so the console line reads cleanly.
        let cleaned = strip_option_debug(&raw);
        match field.name() {
            "message" => {
                self.message = Some(if raw.starts_with('"') { cleaned } else { raw });
            }
            "addr" => self.addr = Some(cleaned),
            other => self.extras.push((other.to_string(), cleaned)),
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
    use super::{LogVerbosity, strip_option_debug};

    #[test]
    fn log_verbosity_eq() {
        // LogVerbosity derives PartialEq/Eq but not Debug, so use plain
        // `assert!` to avoid the Debug bound required by assert_eq!/assert_ne!.
        assert!(LogVerbosity::Info == LogVerbosity::Info);
        assert!(LogVerbosity::Debug == LogVerbosity::Debug);
        assert!(LogVerbosity::Trace == LogVerbosity::Trace);
        assert!(LogVerbosity::Info != LogVerbosity::Debug);
        assert!(LogVerbosity::Debug != LogVerbosity::Trace);
        assert!(LogVerbosity::Info != LogVerbosity::Trace);
    }

    #[test]
    fn strip_option_debug_variants() {
        assert_eq!(strip_option_debug("Some(\"stage\")"), "stage");
        assert_eq!(strip_option_debug("None"), "");
        assert_eq!(strip_option_debug("\"stage\""), "stage");
        assert_eq!(strip_option_debug("stage"), "stage");
    }

    /// A line logged while a prompt owns the terminal must be buffered,
    /// not printed on top of the prompt, and must be replayed once the
    /// prompt returns.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn prompt_defers_console_lines_until_it_returns() {
        let mgr = super::test_support::shared_log_manager().await;
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

    /// Outside a prompt the gate is transparent: nothing is buffered.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn lines_are_not_deferred_without_an_active_prompt() {
        let mgr = super::test_support::shared_log_manager().await;
        tracing::info!(subsystem = "test", "emitted with no prompt up");
        assert!(mgr.inner.progress.gate.pending.lock().is_empty());
    }

    /// Smoke-test that `shared_log_manager` returns a usable `LogManager` and
    /// that calling it multiple times yields the same underlying instance
    /// (i.e. `create` works on both).
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn shared_log_manager_returns_usable_manager() {
        let mgr = super::test_support::shared_log_manager().await;
        // Creating a log file must not panic or error.
        let _log = mgr.create("logmgr-smoke").await.expect("create log");
    }
}
