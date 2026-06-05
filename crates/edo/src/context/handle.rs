//! Read-only handle passed to transforms during execution.
//!
//! A [`Handle`] is a snapshot of the build context that gives transforms
//! access to logging, storage, other transforms, environment farms, and
//! command-line arguments without holding a reference to the full
//! [`Context`](super::Context).

use super::{Addr, ContextResult, Log, LogManager, error};
use crate::console::{Console, ConsoleEvent};
use crate::{
    context::Config,
    environment::{Environment, Farm},
    storage::{Id, Storage},
    transform::Transform,
};
use dashmap::DashMap;
use snafu::OptionExt;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Per-run memoization of [`Transform::get_unique_id`] results.
///
/// The scheduler computes transform ids many times per run — once per node
/// in [`Graph::fetch`](crate::scheduler::Graph::fetch), again from
/// [`run_transform_lifecycle`] per worker, and *recursively* from
/// transforms whose own id depends on dependent transforms' ids (e.g.
/// `script` and `compose`). Without memoization a single shared leaf is
/// re-hashed once per ancestor on every pass.
///
/// The cache is bound to a single [`Scheduler::run`](crate::scheduler::Scheduler::run)
/// invocation by attaching it to a fresh [`Handle`] (see
/// [`Handle::with_id_cache`]). It must NOT outlive the run — between runs
/// transforms may legitimately produce different ids (e.g. `update`
/// re-pinned a source).
#[derive(Clone, Default)]
pub struct IdCache {
    inner: Arc<DashMap<Addr, Id>>,
}

impl IdCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a previously memoized id.
    pub fn get(&self, addr: &Addr) -> Option<Id> {
        self.inner.get(addr).map(|e| e.clone())
    }

    /// Insert an id (idempotent — first writer wins).
    pub fn insert(&self, addr: Addr, id: Id) {
        self.inner.entry(addr).or_insert(id);
    }
}

/// A handle is passed to transforms where it needs to look up
/// things in the transform state.
#[derive(Clone)]
pub struct Handle {
    log: LogManager,
    console: Console,
    config: Config,
    storage: Storage,
    transforms: HashMap<Addr, Transform>,
    farms: HashMap<Addr, Farm>,
    args: HashMap<String, String>,
    cancellation: CancellationToken,
    /// Optional per-run id memoization. `None` for handles created outside
    /// a scheduler run (e.g. `update`, `list`, ad-hoc tooling). The
    /// scheduler attaches a fresh cache via [`Handle::with_id_cache`] at
    /// the start of every run.
    id_cache: Option<IdCache>,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Creates a new `Handle` with the given components.
    pub fn new(
        log: LogManager,
        console: Console,
        config: Config,
        storage: Storage,
        transforms: HashMap<Addr, Transform>,
        farms: HashMap<Addr, Farm>,
        args: HashMap<String, String>,
    ) -> Self {
        Self {
            log,
            console,
            config,
            storage,
            transforms,
            farms,
            args,
            cancellation: CancellationToken::new(),
            id_cache: None,
        }
    }

    /// Returns a clone of this handle with the given [`IdCache`] attached.
    ///
    /// Callers (the scheduler) use this to bind a fresh cache to the
    /// handle for the duration of one run. The cache is reachable from
    /// transforms via [`Handle::id_cache`] so they can memoize recursive
    /// dependency-id lookups.
    pub fn with_id_cache(mut self, cache: IdCache) -> Self {
        self.id_cache = Some(cache);
        self
    }

    /// Returns the per-run id cache if one is attached.
    pub fn id_cache(&self) -> Option<&IdCache> {
        self.id_cache.as_ref()
    }

    /// Returns the project wide configuration nodes
    pub fn config(&self) -> Config {
        self.config.clone()
    }

    /// Returns the cancellation token
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Returns a reference to the log manager.
    pub fn log(&self) -> &LogManager {
        &self.log
    }

    /// Returns a reference to the build-event console.
    pub fn console(&self) -> &Console {
        &self.console
    }

    /// Convenience: emit a [`ConsoleEvent`] through the build console.
    pub fn emit(&self, event: ConsoleEvent) {
        self.console.emit(event);
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
    use crate::context::{Addr, Config, Element, FromElementNoContext, error::ContextError};
    use crate::storage::{Backend, LocalBackend, Storage};
    use std::collections::{BTreeMap, HashMap};
    use tempfile::TempDir;

    /// Build a minimal `Storage` backed by a temporary local directory.
    async fn tmp_storage(dir: &std::path::Path) -> Storage {
        let addr = Addr::parse("//edo-test-cache").unwrap();
        let mut config_map = BTreeMap::new();
        config_map.insert(
            "path".to_string(),
            serde_json::Value::String(dir.to_string_lossy().to_string()),
        );
        let element = Element::builder()
            .addr(addr.clone())
            .kind("local")
            .config(config_map)
            .build();
        let config = Config::load::<&std::path::Path>(None).await.unwrap();
        let local = LocalBackend::new(&element, &config).await.unwrap();
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
            crate::console::Console::new(),
            Config::default(),
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
            crate::console::Console::new(),
            Config::default(),
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
