//! Source subsystem.
//!
//! Defines how source code and dependencies are fetched, cached, and staged
//! into build environments. A [`Source`] knows how to retrieve a single
//! artifact (local path, git repo, OCI image, etc.) while a [`Vendor`]
//! exposes a package registry for semver-based dependency resolution via
//! [`Resolver`].
//!
//! All fallible operations return [`SourceResult`], with failures modelled by
//! [`SourceError`].

use crate::context::{Handle, Log};
use crate::environment::Environment;
use crate::storage::{Artifact, Id, Storage};
use crate::util::Reader;
use arc_handle::arc_handle;
use async_trait::async_trait;
use dashmap::DashMap;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
#[cfg(test)]
use mockall::automock;
use snafu::ensure;
use std::path::Path;
use std::sync::{Arc, LazyLock, Weak};

mod error;
mod require;
mod resolver;
mod vendor;
mod version;

/// Convenience result alias for fallible source operations.
pub type SourceResult<T> = std::result::Result<T, error::SourceError>;
pub use error::SourceError;
pub use require::*;
pub use resolver::*;
pub use vendor::*;
pub use version::*;

/// In-flight source fetches; assumes one storage per process.
static SOURCE_FETCHES: LazyLock<DashMap<String, Weak<Fetch>>> = LazyLock::new(DashMap::new);

struct Fetch {
    key: String,
    result: Shared<BoxFuture<'static, Result<Artifact, Arc<SourceError>>>>,
}

impl Drop for Fetch {
    fn drop(&mut self) {
        SOURCE_FETCHES.remove_if(&self.key, |_, fetch| fetch.strong_count() == 0);
    }
}

/// A source of code or artifacts, whether local or remote.
///
/// Implementations handle fetching and staging for a single kind of source
/// (e.g. git clone, local copy, OCI pull). Use [`Source::cache`] in preference
/// to [`Source::fetch`] to benefit from the local artifact cache.
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Source {
    /// The unique id for this source
    async fn get_unique_id(&self) -> SourceResult<Id>;
    /// Fetch the given source to storage
    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact>;
}

impl Source {
    /// Memoizes this source's id for the current run, including across handle clones.
    pub async fn cached_unique_id(&self, ctx: &Handle) -> SourceResult<Id> {
        if let Some(cache) = ctx.id_cache() {
            let cell = cache.source_cell(self);
            return cell.get_or_try_init(|| self.get_unique_id()).await.cloned();
        }
        self.get_unique_id().await
    }

    /// Caches a source and validates it against the current run's id.
    pub async fn cache_in(&self, log: &Log, ctx: &Handle) -> SourceResult<Artifact> {
        let id = self.cached_unique_id(ctx).await?;
        let artifact = self.cache_id(log, ctx.storage(), &id).await?;
        ensure!(
            artifact.config().id() == &id,
            error::ChangedSnafu {
                expected: Box::new(id),
                actual: Box::new(artifact.config().id().clone()),
            }
        );
        Ok(artifact)
    }

    /// Caches a source, sharing concurrent downloads and fetch outcomes per id.
    pub async fn cache(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        self.cache_id(log, storage, &id).await
    }

    async fn cache_id(&self, log: &Log, storage: &Storage, id: &Id) -> SourceResult<Artifact> {
        if storage.has_local(id).await? {
            return Ok(storage.safe_open(id).await?);
        }
        let key = id.to_string();
        let fetch = {
            let mut entry = SOURCE_FETCHES.entry(key.clone()).or_default();
            if let Some(fetch) = entry.upgrade() {
                fetch
            } else {
                let source = self.clone();
                let storage = storage.clone();
                let log = log.clone();
                let id = id.clone();
                let result = async move {
                    match storage.fetch_source(&id).await? {
                        Some(artifact) => Ok(artifact),
                        None => source.fetch(&log, &storage).await,
                    }
                }
                .map(|result: SourceResult<Artifact>| result.map_err(Arc::new))
                .boxed()
                .shared();
                let fetch = Arc::new(Fetch {
                    key: key.clone(),
                    result,
                });
                *entry = Arc::downgrade(&fetch);
                fetch
            }
        };
        let result = fetch.result.clone().await;
        SOURCE_FETCHES.remove_if(&key, |_, entry| entry.ptr_eq(&Arc::downgrade(&fetch)));
        result.map_err(|source| SourceError::Shared { source })
    }

    /// Probes only the local source cache using the current run's source id.
    pub async fn is_cached(&self, ctx: &Handle) -> SourceResult<bool> {
        let id = self.cached_unique_id(ctx).await?;
        Ok(ctx.storage().has_local(&id).await?)
    }

    /// Helper for staging sources off their layer media_types instead of deferring
    /// to an individual's source stage logic. Transforms may just want flat extracts.
    /// this will also ignore the source specific out transforms
    pub async fn stage_by_mediatype(
        &self,
        ctx: &Handle,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()> {
        let id = self.cached_unique_id(ctx).await?;
        let artifact = ctx.storage().safe_open(&id).await?;
        for layer in artifact.layers() {
            let mut reader = ctx.storage().safe_read(layer).await?;
            if layer.media_type().is_compressed() {
                reader = Reader::with_decompression(reader, &layer.media_type().compression());
            }
            if layer.media_type().is_archive() {
                env.unpack_stream(path, layer.media_type(), reader).await?;
            } else {
                env.write_stream(path, reader).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::logmgr::test_support::shared_log_manager;
    use crate::context::{Addr, Config, Element, FromElementNoContext, Log};
    use crate::storage::{Backend, LocalBackend, Storage};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    async fn make_storage(root: &std::path::Path) -> Storage {
        Storage::init(&make_backend(root).await).await.unwrap()
    }

    async fn make_backend(root: &std::path::Path) -> Backend {
        let addr = Addr::parse("//edo-test-cache").unwrap();
        let mut config_map = BTreeMap::new();
        config_map.insert(
            "path".to_string(),
            serde_json::Value::String(root.to_string_lossy().to_string()),
        );
        let element = Element::builder()
            .addr(addr)
            .kind("local")
            .config(config_map)
            .build();
        let config = Config::load::<&std::path::Path>(None).await.unwrap();
        let local = LocalBackend::new(&element, &config).await.unwrap();
        Backend::new(local)
    }

    struct CountingSource {
        id: Id,
        calls: Arc<AtomicUsize>,
        gate: Option<Arc<tokio::sync::Notify>>,
        fail: bool,
    }

    #[async_trait]
    impl SourceImpl for CountingSource {
        async fn get_unique_id(&self) -> SourceResult<Id> {
            Ok(self.id.clone())
        }
        async fn fetch(&self, _log: &Log, storage: &Storage) -> SourceResult<Artifact> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            if self.fail {
                return Err(SourceError::Io {
                    source: std::io::Error::other("fetch failed"),
                });
            }
            use crate::storage::{Artifact as A, Config as ArtCfg, LayerOptions, MediaType};
            let mut writer = storage.safe_start_layer().await?;
            use tokio::io::AsyncWriteExt;
            writer.write_all(b"x").await.unwrap();
            writer.shutdown().await.unwrap();
            let layer = storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Manifest)
                        .build(),
                )
                .await?;
            let artifact = A::builder()
                .media_type(MediaType::Manifest)
                .config(ArtCfg::builder().id(self.id.clone()).build())
                .layers(vec![layer])
                .build();
            storage.safe_save(&artifact).await?;
            Ok(artifact)
        }
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn singleflight_coalesces_concurrent_cache_calls() {
        let dir = TempDir::new().expect("tempdir");
        let storage = make_storage(dir.path()).await;
        let logmgr = shared_log_manager().await;
        let log = logmgr.create("singleflight-test").await.expect("log");

        let id = Id::builder()
            .name("singleflight_test")
            .digest("0".repeat(64))
            .build();
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let source = Source::new(CountingSource {
            id: id.clone(),
            calls: calls.clone(),
            gate: Some(gate.clone()),
            fail: false,
        });

        let mut handles = Vec::new();
        for _ in 0..10 {
            let src = source.clone();
            let log = log.clone();
            let storage = storage.clone();
            handles.push(tokio::spawn(async move { src.cache(&log, &storage).await }));
        }
        wait_for_callers(&id, 10).await;
        // Cancel the original caller; peers must finish the shared fetch.
        handles.remove(0).abort();
        gate.notify_one();
        for h in handles {
            h.await.expect("join").expect("cache");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "singleflight should collapse 10 concurrent cache() calls to a single fetch"
        );
        assert!(!SOURCE_FETCHES.contains_key(&id.to_string()));
    }

    async fn wait_for_callers(id: &Id, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if SOURCE_FETCHES
                    .get(&id.to_string())
                    .is_some_and(|fetch| fetch.strong_count() == count)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("callers should join the same fetch");
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn singleflight_shares_failure_and_allows_retry() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(dir.path()).await;
        let log = shared_log_manager()
            .await
            .create("singleflight-error")
            .await
            .unwrap();
        let id = Id::builder()
            .name("singleflight_error")
            .digest("1".repeat(64))
            .build();
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let source = Source::new(CountingSource {
            id: id.clone(),
            calls: calls.clone(),
            gate: Some(gate.clone()),
            fail: true,
        });
        let mut handles = Vec::new();
        for _ in 0..10 {
            let source = source.clone();
            let storage = storage.clone();
            let log = log.clone();
            handles.push(tokio::spawn(
                async move { source.cache(&log, &storage).await },
            ));
        }
        wait_for_callers(&id, 10).await;
        gate.notify_one();
        let mut shared_error = None;
        for handle in handles {
            let SourceError::Shared { source } = handle.await.unwrap().unwrap_err() else {
                panic!("expected shared error")
            };
            assert!(source.to_string().contains("fetch failed"));
            if let Some(error) = &shared_error {
                assert!(Arc::ptr_eq(error, &source));
            }
            shared_error = Some(source);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!SOURCE_FETCHES.contains_key(&id.to_string()));
        gate.notify_one();
        assert!(source.cache(&log, &storage).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!SOURCE_FETCHES.contains_key(&id.to_string()));
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn singleflight_reclaims_cancelled_fetch() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(dir.path()).await;
        let log = shared_log_manager()
            .await
            .create("singleflight-cancel")
            .await
            .unwrap();
        let id = Id::builder()
            .name("singleflight_cancel")
            .digest("2".repeat(64))
            .build();
        let source = Source::new(CountingSource {
            id: id.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            gate: Some(Arc::new(tokio::sync::Notify::new())),
            fail: false,
        });
        let handle = tokio::spawn(async move { source.cache(&log, &storage).await });
        wait_for_callers(&id, 1).await;
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(!SOURCE_FETCHES.contains_key(&id.to_string()));
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn singleflight_coalesces_source_cache_downloads() {
        let remote_dir = TempDir::new().unwrap();
        let remote = make_storage(remote_dir.path()).await;
        let local_dir = TempDir::new().unwrap();
        let local = make_storage(local_dir.path()).await;
        let log = shared_log_manager()
            .await
            .create("singleflight-remote")
            .await
            .unwrap();
        let id = Id::builder()
            .name("singleflight_remote")
            .digest("3".repeat(64))
            .build();
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Source::new(CountingSource {
            id: id.clone(),
            calls: calls.clone(),
            gate: None,
            fail: false,
        });
        let artifact = source.fetch(&log, &remote).await.unwrap();
        local
            .add_source_cache("remote", &make_backend(remote_dir.path()).await)
            .await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let fetch = Arc::new(Fetch {
            key: id.to_string(),
            result: {
                let local = local.clone();
                let id = id.clone();
                let gate = gate.clone();
                async move {
                    gate.notified().await;
                    local
                        .fetch_source(&id)
                        .await
                        .map(|artifact| artifact.unwrap())
                        .map_err(|error| Arc::new(SourceError::from(error)))
                }
                .boxed()
                .shared()
            },
        });
        SOURCE_FETCHES.insert(id.to_string(), Arc::downgrade(&fetch));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let source = source.clone();
            let local = local.clone();
            let log = log.clone();
            handles.push(tokio::spawn(
                async move { source.cache(&log, &local).await },
            ));
        }
        wait_for_callers(&id, 11).await;
        assert!(!local.has_local(&id).await.unwrap());
        gate.notify_one();
        for handle in handles {
            assert_eq!(
                handle.await.unwrap().unwrap().config().id(),
                artifact.config().id()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(local.has_local(&id).await.unwrap());
        assert!(!SOURCE_FETCHES.contains_key(&id.to_string()));
    }
}
