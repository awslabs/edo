//! Read-only build context snapshots for transforms.

use super::{Addr, ContextResult, Log, LogManager, error};
use crate::{
    context::Config,
    environment::{Environment, Farm},
    source::Source,
    storage::{Id, Storage},
    transform::{Transform, TransformResult},
};
use dashmap::DashMap;
use snafu::OptionExt;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

type SourceIdEntry = (Source, Arc<OnceCell<Id>>);

/// Memoizes transform and source ids for one scheduler run.
#[derive(Clone, Default)]
pub struct IdCache {
    inner: Arc<DashMap<Addr, Arc<OnceCell<Id>>>>,
    sources: Arc<DashMap<usize, SourceIdEntry>>,
}

impl IdCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    fn transform_cell(&self, addr: &Addr) -> Arc<OnceCell<Id>> {
        self.inner.entry(addr.clone()).or_default().clone()
    }

    pub(crate) fn source_cell(&self, source: &Source) -> Arc<OnceCell<Id>> {
        let key = Arc::as_ptr(source.inner()) as *const () as usize;
        // Retain the source so its allocation cannot be reused during this run.
        self.sources
            .entry(key)
            .or_insert_with(|| (source.clone(), Arc::new(OnceCell::new())))
            .1
            .clone()
    }
}

/// Transform access to build configuration and registered components.
#[derive(Clone)]
pub struct Handle {
    log: LogManager,
    config: Config,
    storage: Storage,
    transforms: HashMap<Addr, Transform>,
    farms: HashMap<Addr, Farm>,
    args: HashMap<String, String>,
    cancellation: CancellationToken,
    /// Optional per-run id memoization.
    id_cache: Option<IdCache>,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Creates a new `Handle` with the given components.
    ///
    /// Share `cancellation` within a session; use a fresh token for independent callers.
    pub fn new(
        log: LogManager,
        config: Config,
        storage: Storage,
        transforms: HashMap<Addr, Transform>,
        farms: HashMap<Addr, Farm>,
        args: HashMap<String, String>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            log,
            config,
            storage,
            transforms,
            farms,
            args,
            cancellation,
            id_cache: None,
        }
    }

    /// Attaches the given per-run id cache.
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

    /// Replaces this handle's token without changing the session token.
    pub(crate) fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
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

    /// Resolves a transform and memoizes its id for the current run.
    pub async fn unique_id_of(&self, addr: &Addr) -> TransformResult<Id> {
        let transform = self
            .get(addr)
            .context(error::NoPluginSnafu { addr: addr.clone() })?;
        if let Some(cache) = &self.id_cache {
            let cell = cache.transform_cell(addr);
            return cell
                .get_or_try_init(|| transform.get_unique_id(self))
                .await
                .cloned();
        }
        transform.get_unique_id(self).await
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
    use super::{Handle, IdCache};
    use crate::context::logmgr::test_support::shared_log_manager;
    use crate::context::{Addr, Config, Element, FromElementNoContext, error::ContextError};
    use crate::source::{MockSourceImpl, Source, SourceError};
    use crate::storage::{
        Artifact, Backend, Config as ArtifactConfig, Id, LocalBackend, MediaType, Storage,
    };
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
            Config::default(),
            storage,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(handle.transforms().is_empty());
        assert!(handle.args().is_empty());

        let addr = Addr::parse("//x").unwrap();
        assert!(handle.get(&addr).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn source_ids_are_shared_within_a_run_and_refreshed_between_runs() {
        let dir = TempDir::new().unwrap();
        let handle = Handle::new(
            shared_log_manager().await,
            Config::default(),
            tmp_storage(dir.path()).await,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            tokio_util::sync::CancellationToken::new(),
        );
        let mut mock = MockSourceImpl::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        mock.expect_get_unique_id().times(2).returning({
            let calls = calls.clone();
            move || {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Id::builder()
                    .name("source")
                    .digest(format!("{n:064x}"))
                    .build())
            }
        });
        let source = Source::new(mock);
        let first_run = handle.clone().with_id_cache(IdCache::new());
        let other_handle = first_run.clone();
        let other_source = source.clone();
        let (first, second) = tokio::join!(
            source.cached_unique_id(&first_run),
            other_source.cached_unique_id(&other_handle),
        );
        let first = first.unwrap();
        assert_eq!(first, second.unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let next_run = handle.with_id_cache(IdCache::new());
        assert_ne!(first, source.cached_unique_id(&next_run).await.unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn source_cache_probe_and_fetch_use_the_memoized_id() {
        let dir = TempDir::new().unwrap();
        let handle = Handle::new(
            shared_log_manager().await,
            Config::default(),
            tmp_storage(dir.path()).await,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            tokio_util::sync::CancellationToken::new(),
        )
        .with_id_cache(IdCache::new());
        let id = Id::builder().name("source").digest("a".repeat(64)).build();
        let artifact = Artifact::builder()
            .media_type(MediaType::Manifest)
            .config(ArtifactConfig::builder().id(id.clone()).build())
            .build();
        let mut mock = MockSourceImpl::new();
        mock.expect_get_unique_id().times(1).returning({
            let id = id.clone();
            move || Ok(id.clone())
        });
        mock.expect_fetch().times(1).returning({
            let artifact = artifact.clone();
            move |_, _| Ok(artifact.clone())
        });
        let source = Source::new(mock);
        assert_eq!(source.cached_unique_id(&handle).await.unwrap(), id);
        assert!(!source.is_cached(&handle).await.unwrap());
        let log = handle.log().create("source-cache-test").await.unwrap();
        let fetched = source.cache_in(&log, &handle).await.unwrap();
        assert_eq!(fetched.config().id(), &id);
        handle.storage().safe_save(&fetched).await.unwrap();
        assert!(source.is_cached(&handle).await.unwrap());
        assert_eq!(
            source.cache_in(&log, &handle).await.unwrap().config().id(),
            &id
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn source_cache_in_rejects_fetch_under_a_different_id() {
        let dir = TempDir::new().unwrap();
        let handle = Handle::new(
            shared_log_manager().await,
            Config::default(),
            tmp_storage(dir.path()).await,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            tokio_util::sync::CancellationToken::new(),
        )
        .with_id_cache(IdCache::new());
        let memoized = Id::builder().name("source").digest("a".repeat(64)).build();
        let changed = Id::builder().name("source").digest("b".repeat(64)).build();
        let artifact = Artifact::builder()
            .media_type(MediaType::Manifest)
            .config(ArtifactConfig::builder().id(changed.clone()).build())
            .build();
        let mut mock = MockSourceImpl::new();
        mock.expect_get_unique_id().times(1).returning({
            let memoized = memoized.clone();
            move || Ok(memoized.clone())
        });
        mock.expect_fetch()
            .times(1)
            .returning(move |_, _| Ok(artifact.clone()));
        let source = Source::new(mock);
        let log = handle.log().create("source-changed-test").await.unwrap();
        match source.cache_in(&log, &handle).await {
            Err(SourceError::Changed { expected, actual }) => {
                assert_eq!(*expected, memoized);
                assert_eq!(*actual, changed);
            }
            other => panic!("expected SourceError::Changed, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn handle_create_environment_no_farm_errors() {
        let dir = TempDir::new().unwrap();
        let log_mgr = shared_log_manager().await;
        let storage = tmp_storage(dir.path()).await;

        let handle = Handle::new(
            log_mgr.clone(),
            Config::default(),
            storage,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            tokio_util::sync::CancellationToken::new(),
        );

        let log_path = dir.path().join("test.log");
        let log = crate::context::Log::new(&log_mgr, &log_path).unwrap();
        let addr = Addr::parse("//missing-farm").unwrap();
        let env_path = dir.path().join("env");

        let result = handle.create_environment(&log, &addr, &env_path).await;

        // Match manually because `Environment` lacks `Debug`.
        match result {
            Err(ContextError::NoEnvironmentFound { .. }) => {}
            Err(other) => panic!("expected NoEnvironmentFound, got: {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }
}
