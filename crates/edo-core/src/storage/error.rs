use snafu::Snafu;
use tokio::task::JoinError;

pub type StorageResult<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum StorageError {
    #[snafu(display("failed to resolve absolute path: {source}"))]
    Absolute { source: std::io::Error },
    #[snafu(display("multiple errors occured: {}", children.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n")))]
    Child { children: Vec<StorageError> },
    #[snafu(display("invalid artifact id: {reason}"))]
    Id { reason: String },
    #[snafu(display("io error occured setting up storage: {source}"))]
    Io { source: std::io::Error },
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(display("{value} is not a valid edo artifact media type"))]
    InvalidMediaType { value: String },
    #[snafu(display("failed to join on task: {source}"))]
    Join { source: JoinError },
    #[snafu(transparent)]
    Project {
        source: crate::context::ContextError,
    },
    #[snafu(display("[FATAL] Built-in regular expression is invalid: {source}"))]
    Regex { source: regex::Error },
    #[snafu(display(
        "this version of edo does not support artifacts with the provided version in {value}"
    ))]
    Schema { value: String },
    #[snafu(display("invalid semantic version: {source}"))]
    Semver { source: semver::Error },
}
