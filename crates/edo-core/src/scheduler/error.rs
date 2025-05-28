use snafu::Snafu;
use tokio::{sync::mpsc::error::SendError, task::JoinError};

use crate::{context::Addr, storage::IdBuilderError};

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum SchedulerError {
    #[snafu(display("failed to recreate artifact id: {source}"))]
    IdBuild { source: IdBuilderError },
    #[snafu(transparent)]
    Cache {
        source: crate::storage::StorageError,
    },
    #[snafu(display("errors occured during execution: {}", children.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n")))]
    Child { children: Vec<SchedulerError> },
    #[snafu(display("dependency does not exist in execution graph: {addr}"))]
    Depend { addr: Addr },
    #[snafu(transparent)]
    Environment {
        source: crate::environment::EnvironmentError,
    },
    #[snafu(display("failed to build execution graph: {source}"))]
    Graph { source: daggy::WouldCycle<String> },
    #[snafu(display("failed to prompt user: {source}"))]
    Inquire { source: dialoguer::Error },
    #[snafu(display("io error: {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("failed to wait for execution tasks: {source}"))]
    Join { source: JoinError },
    #[snafu(display("no transform exists in execution graph with addr: {addr}"))]
    Node { addr: Addr },
    #[snafu(display("transformation didn't run"))]
    NoRun,
    #[snafu(display("{message}"))]
    Passthrough { message: String },
    #[snafu(transparent)]
    Context {
        source: crate::context::ContextError,
    },
    #[snafu(display("no transform matching addr '{addr}' found in project"))]
    ProjectTransform { addr: Addr },
    #[snafu(display("failed to signal task completion: {source}"))]
    Signal {
        source: SendError<daggy::NodeIndex<u32>>,
    },
    #[snafu(display("could not await on a result from a non-building node"))]
    State,
    #[snafu(display("failed to create temporary directory: {source}"))]
    TemporaryDirectory { source: std::io::Error },
    #[snafu(transparent)]
    Transform {
        source: crate::transform::TransformError,
    },
}
