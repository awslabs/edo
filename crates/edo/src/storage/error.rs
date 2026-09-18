use snafu::Snafu;
use tokio::task::JoinError;

/// Convenience result alias for fallible storage operations.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

/// Errors produced by the storage subsystem.
///
/// Covers I/O failures, invalid identifiers, media type parsing, schema
/// version mismatches, and aggregated child-task errors.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum StorageError {
    /// Failed to resolve a filesystem path to an absolute path.
    #[snafu(display("failed to resolve absolute path: {source}"))]
    Absolute { source: std::io::Error },
    /// Multiple storage operations failed concurrently.
    #[snafu(display("multiple errors occured: {}", children.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n")))]
    Child { children: Vec<StorageError> },
    /// An artifact identifier could not be parsed.
    #[snafu(display("invalid artifact id: {reason}"))]
    Id { reason: String },
    /// A general I/O error occurred during storage setup or operation.
    #[snafu(display("io error occured setting up storage: {source}"))]
    Io { source: std::io::Error },
    /// An opaque error surfaced by a backend implementation (plugin or builtin).
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    /// The provided string is not a valid edo artifact media type.
    #[snafu(display("{value} is not a valid edo artifact media type"))]
    InvalidMediaType { value: String },
    /// A spawned async task panicked or was cancelled.
    #[snafu(display("failed to join on task: {source}"))]
    Join { source: JoinError },
    /// A propagated context-layer error (e.g. project/config issues).
    #[snafu(transparent)]
    Project {
        source: crate::context::ContextError,
    },
    /// A built-in regular expression failed to compile (should never happen).
    #[snafu(display("[FATAL] Built-in regular expression is invalid: {source}"))]
    Regex { source: regex::Error },
    /// The artifact was created with an unsupported schema version.
    #[snafu(display(
        "this version of edo does not support artifacts with the provided version in {value}"
    ))]
    Schema { value: String },
    /// A semver version string could not be parsed.
    #[snafu(display("invalid semantic version: {source}"))]
    Semver { source: semver::Error },
    /// No algorithm defined in the digest
    #[snafu(display("digest references require its hash algorithm as a prefix: algorithm:hash"))]
    DigestNoAlgorithm,
    #[snafu(display("invalid base16 encoded string: {source}"))]
    InvalidBase16 { source: base16::DecodeError },
    #[snafu(display("invalid base64 encoded string: {source}"))]
    InvalidBase64 { source: base64::DecodeError },
    #[snafu(display("failed to translate to oci digest: {source}"))]
    OCITranslate { source: ocilot::error::Error },
    /// Unsupported hashing algorithm
    #[snafu(display("unsupported digest algorithm: {algo}"))]
    UnsupportedAlgorithm { algo: String },
    /// Unknown encoding
    #[snafu(display("could not detect base encoding of {input}"))]
    UnknownEncoding { input: String },
}
