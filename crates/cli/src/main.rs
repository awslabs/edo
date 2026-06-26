use clap::Parser;
use cmd::{Checkout, List, Prune, Run, Update};
use std::path::PathBuf;

mod cmd;

pub type Result<T> = std::result::Result<T, error::Error>;

pub mod error {
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("io error: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("no transform registered for address {addr}"))]
        UnknownTransform { addr: String },
        #[snafu(display(
            "cannot checkout artifact layer with media type {media_type}: not supported"
        ))]
        UnsupportedMediaType { media_type: String },
        #[snafu(display("failed to extract zip layer: {source}"))]
        ZipExtract {
            source: edo_core::environment::ZipError,
        },
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
        #[snafu(transparent)]
        Environment {
            source: edo::environment::EnvironmentError,
        },
        #[snafu(transparent)]
        Source { source: edo::source::SourceError },
        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },
        #[snafu(transparent)]
        Core { source: edo_core::error::Error },
    }
}

#[derive(Parser, Debug, Clone)]
#[command(version, about = "Edo build tool", long_about = None)]
pub struct Args {
    #[arg(short, long, default_value = "false")]
    debug: bool,
    #[arg(short, long, default_value = "false")]
    trace: bool,
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[arg(short, long)]
    storage: Option<PathBuf>,
    /// Console rendering mode. Retained for backwards-compatible CLI
    /// invocation but currently a no-op — the new tui has a single
    /// fixed rendering path. Valid values: auto, full, simple, none.
    #[arg(long, default_value = "auto")]
    #[allow(dead_code)]
    console_mode: String,
    /// Path to the JSONL build-event log; pass `none` to disable.
    /// Defaults to `<storage>/events.jsonl`.
    #[arg(long, default_value = "default")]
    event_log: String,
    #[clap(subcommand)]
    command: Commands,
}

impl Args {
    /// Resolve the user-supplied `--event-log` argument to an absolute
    /// path (or `None` if disabled).
    pub fn resolve_event_log(&self) -> Option<PathBuf> {
        if self.event_log.eq_ignore_ascii_case("none") {
            return None;
        }
        if self.event_log != "default" {
            return Some(PathBuf::from(&self.event_log));
        }
        // Default: <storage>/events.jsonl. When `--storage` is not set,
        // fall back to ./.edo/events.jsonl.
        let base = self
            .storage
            .clone()
            .unwrap_or_else(|| PathBuf::from(".edo"));
        Some(base.join("events.jsonl"))
    }
}

#[derive(Parser, Debug, Clone)]
enum Commands {
    Checkout(Checkout),
    Run(Run),
    Prune(Prune),
    Update(Update),
    List(List),
}

#[tokio::main]
#[snafu::report]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Every subcommand installs the TUI console via `create_context`,
    // but historically only `Run` also tore it down (via
    // `Context::run`). The other four (`Checkout`, `Prune`, `Update`,
    // `List`) exited with raw mode still enabled and the ratatui
    // inline viewport not unwound, leaving the user's terminal
    // unusable (no key echo, cursor hidden). Ditto for any error path
    // out of `Run` before `Context::run()` runs — a failure in
    // `create_context` or address parsing would bypass the shutdown.
    //
    // Guarantee shutdown here at the top level: run the subcommand,
    // capture its result, then unconditionally drain the console. The
    // `UI::Drop` impl is a further safety net for panic / cancellation
    // paths, but this is the primary handoff.
    let result = match args.clone().command {
        Commands::Checkout(cmd) => cmd.run(args.clone()).await,
        Commands::Run(cmd) => cmd.run(args.clone()).await,
        Commands::Prune(cmd) => cmd.run(args.clone()).await,
        Commands::Update(cmd) => cmd.run(args.clone()).await,
        Commands::List(cmd) => cmd.run(args.clone()).await,
    };
    if let Some(c) = edo::ui::Console::global() {
        c.shutdown().await;
    }
    result?;
    Ok(())
}
