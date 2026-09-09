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
    /// Path to the JSONL structured log. Pass `none` to disable, `default`
    /// to write to `<logs-dir>/edo.jsonl` (the pre-existing behaviour),
    /// or any other value as an explicit path.
    #[arg(long, default_value = "default")]
    event_log: String,
    #[clap(subcommand)]
    command: Commands,
}

impl Args {
    /// Translate the `--event-log` string into the typed
    /// [`edo::context::EventLog`] the log manager consumes.
    pub fn event_log_setting(&self) -> edo::context::EventLog {
        if self.event_log.eq_ignore_ascii_case("none") {
            edo::context::EventLog::Disabled
        } else if self.event_log == "default" {
            edo::context::EventLog::Default
        } else {
            edo::context::EventLog::Path(PathBuf::from(&self.event_log))
        }
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

    // SIGINT (Ctrl+C) belt-and-braces handler. First tap flips the
    // shared cancellation switch cooperatively (`Context::cancellation`
    // observers see it between lifecycle stages). Second tap hard-exits
    // 130 (128 + SIGINT) so the user always has an escape hatch even if
    // a worker is stuck in a syscall.
    //
    // The `CancellationToken` is published to `Context::init` via the
    // shared `edo::context::install_cancellation` slot below, so both
    // this watcher and the scheduler flip the same switch.
    let sigint_token = edo::context::install_cancellation();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        sigint_token.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });

    let result = match args.clone().command {
        Commands::Checkout(cmd) => cmd.run(args.clone()).await,
        Commands::Run(cmd) => cmd.run(args.clone()).await,
        Commands::Prune(cmd) => cmd.run(args.clone()).await,
        Commands::Update(cmd) => cmd.run(args.clone()).await,
        Commands::List(cmd) => cmd.run(args.clone()).await,
    };
    result?;
    Ok(())
}
