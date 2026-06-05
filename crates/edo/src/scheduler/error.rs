//! Error types for the scheduler subsystem.

use snafu::Snafu;
use tokio::{sync::mpsc::error::SendError, task::JoinError};

use crate::context::Addr;

/// Errors that can occur during task scheduling and execution.
#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum SchedulerError {
    #[snafu(transparent)]
    Cache {
        source: crate::storage::StorageError,
    },
    #[snafu(display("execution was cancelled"))]
    Cancelled,
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
    #[snafu(display("FATAL: infallible error occured in scheduler"))]
    Infallable,
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
    #[snafu(display("expected a subgraph membership that did not exist"))]
    Subgraph,
    #[snafu(display("failed to create temporary directory: {source}"))]
    TemporaryDirectory { source: std::io::Error },
    #[snafu(transparent)]
    Transform {
        source: crate::transform::TransformError,
    },
}

#[cfg(test)]
mod tests {
    //! Display-output tests for [`SchedulerError`] variants.
    //!
    //! Each `#[snafu(display(...))]` message is pinned here so that a
    //! refactor of the display string breaks a test rather than silently
    //! changing user-facing output.

    use super::*;

    fn addr() -> Addr {
        Addr::parse("//proj/name").expect("addr parse")
    }

    #[test]
    fn display_depend() {
        let e = SchedulerError::Depend { addr: addr() };
        assert_eq!(
            e.to_string(),
            "dependency does not exist in execution graph: //proj/name",
        );
    }

    #[test]
    fn display_node() {
        let e = SchedulerError::Node { addr: addr() };
        assert_eq!(
            e.to_string(),
            "no transform exists in execution graph with addr: //proj/name",
        );
    }

    #[test]
    fn display_no_run() {
        let e = SchedulerError::NoRun;
        assert_eq!(e.to_string(), "transformation didn't run");
    }

    #[test]
    fn display_passthrough() {
        let e = SchedulerError::Passthrough {
            message: "boom".into(),
        };
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn display_project_transform() {
        let e = SchedulerError::ProjectTransform { addr: addr() };
        assert_eq!(
            e.to_string(),
            "no transform matching addr '//proj/name' found in project",
        );
    }

    #[test]
    fn display_state() {
        let e = SchedulerError::State;
        assert_eq!(
            e.to_string(),
            "could not await on a result from a non-building node",
        );
    }

    #[test]
    fn display_child_aggregates_newline_separated() {
        let e = SchedulerError::Child {
            children: vec![
                SchedulerError::NoRun,
                SchedulerError::Passthrough {
                    message: "second".into(),
                },
            ],
        };
        // Header plus newline-joined child messages.
        assert_eq!(
            e.to_string(),
            "errors occured during execution: transformation didn't run\nsecond",
        );
    }

    #[test]
    fn display_child_empty_children_still_has_header() {
        let e = SchedulerError::Child { children: vec![] };
        assert_eq!(e.to_string(), "errors occured during execution: ");
    }

    /// Compile-time smoke test: the snafu context selectors exist and
    /// build a `SchedulerError` of the expected variant.
    #[test]
    fn snafu_context_selectors_compile() {
        use snafu::IntoError;
        // `NoRunSnafu` takes no fields.
        let e: SchedulerError = NoRunSnafu.build();
        assert!(matches!(e, SchedulerError::NoRun));

        let e: SchedulerError = StateSnafu.build();
        assert!(matches!(e, SchedulerError::State));

        let e: SchedulerError = PassthroughSnafu {
            message: "x".to_string(),
        }
        .build();
        assert!(matches!(e, SchedulerError::Passthrough { .. }));

        // `IntoError` path — take a source-bearing selector and convert.
        let io = std::io::Error::other("io");
        let e: SchedulerError = IoSnafu.into_error(io);
        assert!(matches!(e, SchedulerError::Io { .. }));
    }
}
