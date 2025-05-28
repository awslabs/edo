use snafu::Snafu;
use tracing_subscriber::util::TryInitError;

use super::Addr;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum ContextError {
    #[snafu(display("expected a field named '{field}' with a type of {type_}"))]
    Field { field: String, type_: String },
    #[snafu(display("failed to find home directory"))]
    Home,
    #[snafu(display("io error occured: {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("dependencies have changed, run edo update to update the lockfile"))]
    DependencyChange,
    #[snafu(display("failed to initialize logging: {source}"))]
    Log { source: TryInitError },
    #[snafu(display("lockfile is missing resolution data for: {addr}"))]
    MalformedLock { addr: Addr },
    #[snafu(display("could not read to a configuration node"))]
    Node,
    #[snafu(display("node is missing required keys {}", keys.join(", ")))]
    NodeMissingKeys { keys: Vec<String> },
    #[snafu(display("node is missing a kind definition"))]
    NodeNoKind,
    #[snafu(display("node is missing a name"))]
    NodeNoName,
    #[snafu(display("node is missing an id"))]
    NodeNoId,
    #[snafu(display("could not determine block id"))]
    NoBlockId,
    #[snafu(display("block is not an environment definition"))]
    NotEnvironment,
    #[snafu(display("no environment found with addr '{addr}'"))]
    NoEnvironmentFound { addr: Addr },
    #[snafu(display("no plugin loaded with addr '{addr}'"))]
    NoPlugin { addr: Addr },
    #[snafu(display("block is not a transform definition"))]
    NotTransform,
    #[snafu(display("'{id}' is not a valid block id for a source definition"))]
    NotValidSource { id: String },
    #[snafu(display("block is not a vendor definition"))]
    NotVendor,
    #[snafu(display("failed to parse barkml: {source}"))]
    Parse {
        #[snafu(source(from(barkml::Error, Box::new)))]
        source: Box<barkml::Error>,
    },
    #[snafu(transparent)]
    Plugin {
        source: crate::plugin::error::PluginError,
    },
    #[snafu(transparent)]
    Environment {
        source: crate::environment::EnvironmentError,
    },
    #[snafu(transparent)]
    Scheduler {
        #[snafu(source(from(crate::scheduler::error::SchedulerError, Box::new)))]
        source: Box<crate::scheduler::error::SchedulerError>,
    },
    #[snafu(display("failed to serialize to json: {source}"))]
    Serialize { source: serde_json::Error },
    #[snafu(display("failed to handle starlark build file: {reason}"))]
    Starlark { reason: String },
    #[snafu(transparent)]
    Storage {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    #[snafu(transparent)]
    Transform {
        source: crate::transform::TransformError,
    },
    #[snafu(transparent)]
    Source { source: crate::source::SourceError },
}

impl From<starlark::Error> for ContextError {
    fn from(value: starlark::Error) -> Self {
        Self::Starlark {
            reason: value.to_string(),
        }
    }
}
