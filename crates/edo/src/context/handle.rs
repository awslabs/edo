//! Read-only handle passed to transforms during execution.
//!
//! A [`Handle`] is a snapshot of the build context that gives transforms
//! access to logging, storage, other transforms, environment farms, and
//! command-line arguments without holding a reference to the full
//! [`Context`](super::Context).

use super::{Addr, ContextResult, Log, LogManager, error};
use crate::{
    environment::{Environment, Farm},
    storage::Storage,
    transform::Transform,
};
use snafu::OptionExt;
use std::collections::HashMap;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// A handle is passed to transforms where it needs to look up
/// things in the transform state.
#[derive(Clone)]
pub struct Handle {
    log: LogManager,
    storage: Storage,
    transforms: HashMap<Addr, Transform>,
    farms: HashMap<Addr, Farm>,
    args: HashMap<String, String>,
    cancellation: CancellationToken,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Creates a new `Handle` with the given components.
    pub fn new(
        log: LogManager,
        storage: Storage,
        transforms: HashMap<Addr, Transform>,
        farms: HashMap<Addr, Farm>,
        args: HashMap<String, String>,
    ) -> Self {
        Self {
            log,
            storage,
            transforms,
            farms,
            args,
            cancellation: CancellationToken::new(),
        }
    }

    /// Returns the cancellation token
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Returns a reference to the log manager.
    pub fn log(&self) -> &LogManager {
        &self.log
    }

    /// Returns a reference to the storage backend.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Looks up a transform by address, returning a clone if found.
    pub fn get(&self, addr: &Addr) -> Option<Transform> {
        self.transforms.get(addr).cloned()
    }

    /// Returns a reference to the full transforms map.
    pub fn transforms(&self) -> &HashMap<Addr, Transform> {
        &self.transforms
    }

    /// Returns a reference to the command-line arguments map.
    pub fn args(&self) -> &HashMap<String, String> {
        &self.args
    }

    /// Creates a new build environment from the farm registered at `addr`.
    pub async fn create_environment(
        &self,
        log: &Log,
        addr: &Addr,
        path: &Path,
    ) -> ContextResult<Environment> {
        let farm = self
            .farms
            .get(addr)
            .context(error::NoEnvironmentFoundSnafu { addr: addr.clone() })?;
        let env = farm.create(log, path).await?;
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use super::Handle;
    use crate::context::logmgr::test_support::shared_log_manager;
    use crate::context::{Addr, Config, Node, error::ContextError};
    use crate::storage::{Backend, LocalBackend, Storage};
    use std::collections::{BTreeMap, HashMap};
    use tempfile::TempDir;

    /// Build a minimal `Storage` backed by a temporary local directory.
    async fn tmp_storage(dir: &std::path::Path) -> Storage {
        let addr = Addr::parse("//edo-test-cache").unwrap();
        let mut table = BTreeMap::new();
        table.insert(
            "path".to_string(),
            Node::new_string(dir.to_string_lossy().to_string()),
        );
        let node = Node::new_definition("storage", "local", "test", table);
        let config = Config::load::<&std::path::Path>(None).await.unwrap();
        let local = <LocalBackend as crate::context::DefinableNoContext<
            crate::storage::StorageError,
            crate::context::NonConfigurable<crate::storage::StorageError>,
        >>::new(&addr, &node, &config)
        .await
        .unwrap();
        Storage::init(&Backend::new(local)).await.unwrap()
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn handle_accessors_return_passed_values() {
        let dir = TempDir::new().unwrap();
        let log_mgr = shared_log_manager().await;
        let storage = tmp_storage(dir.path()).await;

        let handle = Handle::new(
            log_mgr,
            storage,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );

        assert!(handle.transforms().is_empty());
        assert!(handle.args().is_empty());

        let addr = Addr::parse("//x").unwrap();
        assert!(handle.get(&addr).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn handle_create_environment_no_farm_errors() {
        let dir = TempDir::new().unwrap();
        let log_mgr = shared_log_manager().await;
        let storage = tmp_storage(dir.path()).await;

        let handle = Handle::new(
            log_mgr.clone(),
            storage,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );

        let log_path = dir.path().join("test.log");
        let log = crate::context::Log::new(&log_mgr, &log_path).unwrap();
        let addr = Addr::parse("//missing-farm").unwrap();
        let env_path = dir.path().join("env");

        let result = handle.create_environment(&log, &addr, &env_path).await;

        // `Environment` doesn't implement Debug so we can't call unwrap_err();
        // match the result manually instead.
        match result {
            Err(ContextError::NoEnvironmentFound { .. }) => {}
            Err(other) => panic!("expected NoEnvironmentFound, got: {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }
}
