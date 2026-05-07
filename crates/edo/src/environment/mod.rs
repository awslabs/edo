//! Environment subsystem.
//!
//! Defines where transforms execute. An [`Environment`] provides sandboxing,
//! filesystem operations, and command execution; a [`Farm`] creates fresh
//! environments on demand for the scheduler. [`Command`] captures a deferred
//! script (interpreter + handlebars-templated commands + variables) that is
//! later dispatched to an [`Environment`] via [`Environment::run`].
//!
//! All fallible operations return [`EnvResult`], with failures modelled by
//! [`EnvironmentError`] in [`error`].

use super::storage::Id;
use super::storage::Storage;
use crate::context::Log;
use crate::util::{Reader, Writer};
use arc_handle::arc_handle;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

mod command;
pub mod error;
mod farm;
mod vfs;

pub use command::*;
pub use error::EnvironmentError;
pub use farm::*;
pub use vfs::*;

/// Convenience result alias for fallible environment operations.
pub type EnvResult<T> = std::result::Result<T, error::EnvironmentError>;

/// An Environment represents where a transform is executed and generally outside of local environments provide some level of sandboxing
/// and isolation.
#[arc_handle]
#[async_trait]
pub trait Environment {
    /// Expand the provided path to a canonicalized absolute path inside of an environment
    async fn expand(&self, path: &Path) -> EnvResult<PathBuf>;
    /// Create a directory inside of the environment
    async fn create_dir(&self, path: &Path) -> EnvResult<()>;
    /// Set an environment variable
    async fn set_env(&self, key: &str, value: &str) -> EnvResult<()>;
    /// Get an environment variable
    async fn get_env(&self, key: &str) -> Option<String>;
    /// Setup the environment for execution
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()>;
    /// Spin the environment up
    async fn up(&self, log: &Log) -> EnvResult<()>;
    /// Spin the environment down
    async fn down(&self, log: &Log) -> EnvResult<()>;
    /// Cleanup the environment
    async fn clean(&self, log: &Log) -> EnvResult<()>;
    /// Write a file into the environment from a given reader
    async fn write(&self, path: &Path, reader: Reader) -> EnvResult<()>;
    /// Unpack an archive into the environment from a given reader
    async fn unpack(&self, path: &Path, reader: Reader) -> EnvResult<()>;
    /// Read or archive a path in the environment to a given writer
    async fn read(&self, path: &Path, writer: Writer) -> EnvResult<()>;
    /// Run a single command in the environment
    async fn cmd(&self, log: &Log, id: &Id, path: &Path, command: &str) -> EnvResult<bool>;
    /// Run a deferred command in the environment
    async fn run(&self, log: &Log, id: &Id, path: &Path, command: &Command) -> EnvResult<bool>;
    /// Open a shell in the environment
    fn shell(&self, path: &Path) -> EnvResult<()>;
}

impl Environment {
    /// Create a new deferred [`Command`] bound to this environment.
    ///
    /// The returned command can accumulate script steps and template variables
    /// before being dispatched via [`Command::send`].
    pub fn defer_cmd(&self, log: &Log, id: &Id) -> Command {
        Command::new(log, id, self)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`Environment::defer_cmd`] and the [`EnvResult`] alias.
    //!
    //! Mock [`EnvironmentImpl`] is duplicated inline here to keep the module
    //! self-contained, mirroring the `scheduler/graph.rs` vs
    //! `scheduler/execute.rs` convention (the project deliberately does not
    //! extract a shared `test_support.rs`).
    use super::*;
    use crate::context::Log;
    use crate::context::test_support::shared_log_manager;
    use crate::environment::error::EnvironmentError;
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct MockEnvImpl;

    #[async_trait]
    impl EnvironmentImpl for MockEnvImpl {
        async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
            Ok(path.to_path_buf())
        }
        async fn create_dir(&self, _p: &Path) -> EnvResult<()> {
            Ok(())
        }
        async fn set_env(&self, _k: &str, _v: &str) -> EnvResult<()> {
            Ok(())
        }
        async fn get_env(&self, _k: &str) -> Option<String> {
            None
        }
        async fn setup(&self, _log: &Log, _storage: &Storage) -> EnvResult<()> {
            Ok(())
        }
        async fn up(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn down(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn clean(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn write(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn unpack(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn read(&self, _p: &Path, _w: Writer) -> EnvResult<()> {
            Ok(())
        }
        async fn cmd(&self, _log: &Log, _id: &Id, _p: &Path, _c: &str) -> EnvResult<bool> {
            Ok(true)
        }
        async fn run(&self, _log: &Log, _id: &Id, _p: &Path, _c: &Command) -> EnvResult<bool> {
            Ok(true)
        }
        fn shell(&self, _p: &Path) -> EnvResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn defer_cmd_returns_command_with_default_interpreter() {
        let dir = TempDir::new().unwrap();
        let mgr = shared_log_manager().await;
        let log = Log::new(&mgr, dir.path().join("defer.log")).unwrap();
        let id = Id::builder()
            .name("defer-test".to_string())
            .digest("deadbeef".to_string())
            .build();
        let env = Environment::new(MockEnvImpl);

        let cmd = env.defer_cmd(&log, &id);
        assert_eq!(cmd.to_string(), "#!/usr/bin/env bash\n");
    }

    #[test]
    fn env_result_is_result_alias() {
        // Compile-time proof that `EnvResult<T>` is structurally
        // `Result<T, EnvironmentError>` — these lines must type-check.
        let ok: EnvResult<()> = Ok(());
        assert!(ok.is_ok());
        let err: EnvResult<u32> = Err(EnvironmentError::Run);
        assert!(err.is_err());
    }
}
