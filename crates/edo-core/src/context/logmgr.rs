use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::Local;
use indicatif::{ProgressState, ProgressStyle};
use owo_colors::{OwoColorize, Stream};
use parking_lot::{Mutex, MutexGuard};
use rand::{rng, Rng};
use snafu::ResultExt;
use tokio::fs::{create_dir_all, remove_dir_all};
use tracing::{
    field::{Field, Visit},
    level_filters::LevelFilter,
    Event, Level, Subscriber,
};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{
    field::RecordFields,
    filter::Targets,
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields, FormattedFields},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    Layer,
};

pub use super::Log;
use super::{error, ContextResult as Result};

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

#[derive(PartialEq, Eq)]
pub enum LogVerbosity {
    Trace,
    Debug,
    Info,
}

#[derive(Clone)]
pub struct LogManager {
    inner: Arc<Inner>,
}

impl LogManager {
    pub async fn init<P: AsRef<Path>>(path: P, verbosity: LogVerbosity) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner::init(path, verbosity).await?),
        })
    }

    pub async fn create(&self, id: &str) -> Result<Log> {
        self.inner.create(self, id).await
    }

    pub fn acquire(&self) -> MutexGuard<()> {
        self.inner.acquire()
    }

    pub async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}

struct Inner {
    path: PathBuf,
    lock: Mutex<()>,
}

pub fn elapsed_subsec(state: &ProgressState, writer: &mut dyn std::fmt::Write) {
    let seconds = state.elapsed().as_secs();
    let sub_seconds = (state.elapsed().as_millis() % 1000) / 100;
    let _ = writer.write_str(&format!("{}.{}s", seconds, sub_seconds));
}

#[instrument]
pub async fn build_sub_unit(sub_unit: u64) {
    let sleep_time = rng().random_range(Duration::from_millis(5000)..Duration::from_millis(10000));
    tokio::time::sleep(sleep_time).await;

    if rng().random_bool(0.2) {
        info!("sub_unit did something!");
    }
}

#[instrument]
pub async fn build(unit: u64) {
    let sleep_time = rng().random_range(Duration::from_millis(2500)..Duration::from_millis(5000));
    tokio::time::sleep(sleep_time).await;

    let rand_num: f64 = rng().random();

    if rand_num < 0.1 {
        tokio::join!(build_sub_unit(0), build_sub_unit(1), build_sub_unit(2));
    } else if rand_num < 0.3 {
        tokio::join!(build_sub_unit(0), build_sub_unit(1));
    } else {
        build_sub_unit(0).await;
    }
}

impl Inner {
    pub async fn init<P: AsRef<Path>>(path: P, verbosity: LogVerbosity) -> Result<Self> {
        let logdir = path.as_ref();
        if logdir.exists() {
            // If the logdir already exists we want to clean it up, it should only be used for a single run
            remove_dir_all(&logdir).await.context(error::IoSnafu)?;
        }
        create_dir_all(&logdir).await.context(error::IoSnafu)?;
        let indicatif_layer = IndicatifLayer::new()
            .with_progress_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] {span_child_prefix} {cmd} {span_fields} {span_name} {msg} {spinner:.green}",
            )
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"])
            .with_key(
                "cmd",
                |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                    let elapsed = state.elapsed();

                    if elapsed > Duration::from_secs(15 * 60) {
                        // Red
                        let _ = write!(writer, "{}", "RUN  ".if_supports_color(Stream::Stderr, |text| text.bold().bright_red().to_string()));
                    } else if elapsed > Duration::from_secs(5 * 60) {
                        // Yellow
                        let _ = write!(writer, "{}", "RUN  ".if_supports_color(Stream::Stderr, |text| text.bold().bright_yellow().to_string()));
                    } else {
                        let _ = write!(writer, "{}", "RUN  ".if_supports_color(Stream::Stderr, |text| text.bold().bright_blue().to_string()));
                    }
                },
            )
            .with_key(
                "color_end",
                |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                    if state.elapsed() > Duration::from_secs(4) {
                        let _ =write!(writer, "\x1b[0m");
                    }
                },
            ),
        ).with_span_child_prefix_symbol("↳ ").with_span_child_prefix_indent("  ").with_max_progress_bars(100, None).with_span_field_formatter(TaskFormatter);

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
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(TaskFormatter)
                    .fmt_fields(TaskFormatter)
                    .with_writer(indicatif_layer.get_stdout_writer())
                    .with_filter(filter.clone()),
            )
            .with(indicatif_layer.with_filter(filter.clone()))
            .try_init()
            .context(error::LogSnafu)?;
        Ok(Self {
            path: logdir.to_path_buf(),
            lock: Mutex::new(()),
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

    pub fn acquire(&self) -> MutexGuard<()> {
        self.lock.lock()
    }
}

#[derive(Default, Clone)]
struct TaskFormatter;

impl<S, N> FormatEvent<S, N> for TaskFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let level = *meta.level();
        let mut depth = 0;

        // Compute the span depth
        if let Some(scope) = ctx.lookup_current() {
            for _ in scope.scope() {
                depth += 1;
            }
        }

        // Create an indentation string based on depth
        let indent = "  ".repeat(depth);

        // Format the timestamp
        let timestamp = Local::now().format("%H:%M:%S").to_string();

        // Apply indentation after timestamp
        write!(
            writer,
            "[{}] {}{} {} ",
            timestamp,
            indent,
            if indent.is_empty() { "" } else { "↳ " },
            match level {
                Level::ERROR => "ERROR"
                    .if_supports_color(Stream::Stdout, |text| text.bold().red().to_string())
                    .to_string(),
                Level::WARN => "WARN "
                    .if_supports_color(Stream::Stdout, |text| text.bold().yellow().to_string())
                    .to_string(),
                Level::INFO => "INFO "
                    .if_supports_color(Stream::Stdout, |text| text.bold().green().to_string())
                    .to_string(),
                Level::DEBUG => "DEBUG"
                    .if_supports_color(Stream::Stdout, |text| text.bold().blue().to_string())
                    .to_string(),
                Level::TRACE => "TRACE"
                    .if_supports_color(Stream::Stdout, |text| text.bold().cyan().to_string())
                    .to_string(),
            }
        )?;

        // Now we print out our fields
        let span = event
            .parent()
            .and_then(|id| ctx.span(id))
            .or_else(|| ctx.lookup_current());
        let scope = span.into_iter().flat_map(|span| span.scope());
        for span in scope {
            let ext = span.extensions();
            let fields = ext.get::<FormattedFields<N>>().unwrap();
            write!(writer, "{}", fields)?;
        }
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)?;

        Ok(())
    }
}

impl<'a> FormatFields<'a> for TaskFormatter {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'a>,
        fields: R,
    ) -> std::fmt::Result {
        let mut task_visitor = TaskVisitor {
            writer: writer.by_ref(),
        };
        fields.record(&mut task_visitor);
        Ok(())
    }
}

struct TaskVisitor<'a> {
    writer: Writer<'a>,
}

impl Visit for TaskVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "addr" {
            let _ = self.writer.write_fmt(format_args!(
                "{} → ",
                value.if_supports_color(Stream::Stdout, |text| text.bold().to_string())
            ));
        } else if field.name() != "message" {
            let _ = self.writer.write_fmt(format_args!(
                " {}={} ",
                field
                    .name()
                    .if_supports_color(Stream::Stdout, |text| text.bold().to_string()),
                value
            ));
        } else {
            let _ = self.writer.write_str(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "addr" {
            let _ = self.writer.write_fmt(format_args!(
                "{} →",
                format!("{:?}", value)
                    .if_supports_color(Stream::Stdout, |text| text.bold().to_string())
            ));
        } else if field.name() != "message" {
            let _ = self.writer.write_fmt(format_args!(
                " {} = {:?} ",
                field
                    .name()
                    .if_supports_color(Stream::Stdout, |text| text.bold().to_string()),
                value
            ));
        } else {
            let _ = self.writer.write_fmt(format_args!("{:?}", value));
        }
    }
}
