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
    #[clap(subcommand)]
    command: Commands,
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

    match args.clone().command {
        Commands::Checkout(cmd) => cmd.run(args.clone()).await?,
        Commands::Run(cmd) => cmd.run(args.clone()).await?,
        Commands::Prune(cmd) => cmd.run(args.clone()).await?,
        Commands::Update(cmd) => cmd.run(args.clone()).await?,
        Commands::List(cmd) => cmd.run(args.clone()).await?,
    }
    Ok(())
}
