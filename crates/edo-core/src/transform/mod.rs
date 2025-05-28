use crate::context::{Addr, Handle, Log};
use crate::def_trait;
use crate::environment::Environment;
use crate::storage::{Artifact, Id};
use std::path::PathBuf;

pub type TransformResult<T> = std::result::Result<T, error::TransformError>;
pub use error::TransformError;

def_trait! {
    "Defines the interface that all transforms must follow" =>
    "A Transform represents a single action to mutate an artifact" =>
    Transform: TransformImpl {
        "Returns the address of the environment farm to use" =>
        environment() -> TransformResult<Addr>;
        "Return the transforms unique id that will represent its output" =>
        get_unique_id(ctx: &Handle) -> TransformResult<Id>;
        "Returns all dependent transforms of this one" =>
        depends() -> TransformResult<Vec<Addr>>;
        "Prepare the transform by fetching all sources and dependent artifacts" =>
        prepare(log: &Log, ctx: &Handle) -> TransformResult<()>;
        "Stage all needed files into the environment" =>
        stage(log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()>;
        "Perform the tranformation" =>
        transform(log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus
        : "Can a user enter a shell if this transform fails" =>
        can_shell() -> bool;
        : "Open a shell in the environment at the appropriate location" =>
        shell(env: &Environment) -> TransformResult<()>
    }
}

pub enum TransformStatus {
    Success(Artifact),
    Retryable(Option<PathBuf>, error::TransformError),
    Failed(Option<PathBuf>, error::TransformError),
}

pub mod error {
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum TransformError {
        #[snafu(transparent)]
        Implementation {
            source: Box<dyn snafu::Error + Send + Sync>,
        },
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(crate::context::ContextError, Box::new)))]
            source: Box<crate::context::ContextError>,
        },
        #[snafu(transparent)]
        Environment {
            #[snafu(source(from(crate::environment::EnvironmentError, Box::new)))]
            source: Box<crate::environment::EnvironmentError>,
        },
        #[snafu(transparent)]
        Source {
            #[snafu(source(from(crate::source::SourceError, Box::new)))]
            source: Box<crate::source::SourceError>,
        },
        #[snafu(transparent)]
        Storage {
            #[snafu(source(from(crate::storage::StorageError, Box::new)))]
            source: Box<crate::storage::StorageError>,
        },
    }
}

#[macro_export]
macro_rules! transform_err {
    ($expr: expr) => {
        match $expr {
            Ok(data) => data,
            Err(e) => {
                error!("wrapped error occured: {e}");
                return TransformStatus::Failed(None, e.into());
            }
        }
    };
}

pub use transform_err;
