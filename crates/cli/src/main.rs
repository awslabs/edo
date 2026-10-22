use clap::Parser;
use cmd::{Checkout, List, Prune, Run, Update};
use edo::context::LogManager;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod cmd;

pub type Result<T> = std::result::Result<T, error::Error>;

pub mod error {
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("command cancelled"))]
        Cancelled,
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
        #[snafu(display(
            "artifact staging path hint {hint:?} is unsafe: it would escape the output root"
        ))]
        UnsafePathHint { hint: std::path::PathBuf },
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
    /// to write to `<storage>/logs/edo.jsonl`, or any other value as an
    /// explicit path.
    #[arg(long, default_value = "default")]
    event_log: edo::context::EventLog,
    #[clap(subcommand)]
    command: Commands,
    #[arg(skip)]
    cancellation: CancellationToken,
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

    // First Ctrl+C cancels; second exits 130 without cleanup.
    let sigint_token = args.cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        sigint_token.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            LogManager::shutdown();
            std::process::exit(130);
        }
    });

    let grace = if matches!(args.command, Commands::Run(_)) {
        Duration::from_secs(5)
    } else {
        Duration::ZERO
    };
    let command = async {
        match args.clone().command {
            Commands::Checkout(cmd) => cmd.run(args.clone()).await,
            Commands::Run(cmd) => cmd.run(args.clone()).await,
            Commands::Prune(cmd) => cmd.run(args.clone()).await,
            Commands::Update(cmd) => cmd.run(args.clone()).await,
            Commands::List(cmd) => cmd.run(args.clone()).await,
        }
    };
    let result = run_cancellable(command, &args.cancellation, grace).await;
    LogManager::shutdown();
    result
}

async fn run_cancellable(
    command: impl std::future::Future<Output = Result<()>>,
    token: &CancellationToken,
    grace: Duration,
) -> Result<()> {
    tokio::pin!(command);
    tokio::select! {
        result = &mut command => result,
        _ = token.cancelled() => {
            // Allow bounded cooperative teardown before dropping the command.
            if !grace.is_zero() {
                let _ = tokio::time::timeout(grace, &mut command).await;
            }
            error::CancelledSnafu.fail()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_stops_non_build_commands() {
        let token = CancellationToken::new();
        let command = async {
            token.cancel();
            std::future::pending::<Result<()>>().await
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_cancellable(command, &token, Duration::ZERO),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(error::Error::Cancelled)));
    }

    #[tokio::test]
    async fn cancellation_allows_cooperative_build_cleanup() {
        let token = CancellationToken::new();
        let cleaned = std::sync::atomic::AtomicBool::new(false);
        let command = async {
            token.cancel();
            tokio::task::yield_now().await;
            cleaned.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let _ = run_cancellable(command, &token, Duration::from_secs(1)).await;
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_bounds_unresponsive_builds() {
        let token = CancellationToken::new();
        token.cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_cancellable(std::future::pending(), &token, Duration::from_millis(10)),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(error::Error::Cancelled)));
    }

    #[tokio::test]
    async fn command_errors_are_preserved() {
        let result = run_cancellable(
            async { error::UnknownTransformSnafu { addr: "//missing" }.fail() },
            &CancellationToken::new(),
            Duration::ZERO,
        )
        .await;
        assert!(matches!(result, Err(error::Error::UnknownTransform { .. })));
    }
}
