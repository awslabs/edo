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
#[cfg(test)]
use mockall::automock;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;

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

/// Global singleflight map for [`Source::cache`].
///
/// Keyed by the artifact's [`Id`]-string. When multiple transforms share
/// a source and all miss the local cache at the same time, only the
/// first caller runs `fetch`; peers wait on the mutex and then observe
/// the artifact in the local cache on the retry pass.
///
/// The map is process-global, not per-`Storage`, because `Storage` is
/// itself effectively a singleton per `Context` (which is per process
/// in every current caller). A per-`Storage` map would require plumbing
/// through the `Source` trait signature, which is a wider API change
/// than justified.
static SOURCE_FETCH_LOCKS: LazyLock<DashMap<String, Arc<Mutex<()>>>> = LazyLock::new(DashMap::new);

fn fetch_lock(id: &Id) -> Arc<Mutex<()>> {
    let key = id.to_string();
    SOURCE_FETCH_LOCKS
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
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
    /// Check the cache if this source already exists, and only if it does not
    /// call fetch to get the artifact. Use this in most cases instead of calling
    /// fetch() as fetch will ALWAYS repull the source.
    ///
    /// A per-`Id` singleflight guards `fetch` so multiple concurrent
    /// callers requesting the same source collapse to a single network
    /// operation. Peers wait on the mutex; when they resume, they hit
    /// the `storage.fetch_source` fast path and skip the redundant
    /// fetch entirely.
    pub async fn cache(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        // Fast path: already in a local/source cache.
        if let Some(artifact) = storage.fetch_source(&id).await? {
            return Ok(artifact);
        }
        // Slow path: coalesce concurrent fetches for this Id.
        let lock = fetch_lock(&id);
        let guard = lock.lock().await;
        // Re-check under the lock: a peer may have completed the
        // fetch while we were waiting.
        let result = if let Some(artifact) = storage.fetch_source(&id).await? {
            Ok(artifact)
        } else {
            self.fetch(log, storage).await
        };
        // Release the lock before running best-effort map cleanup so
        // any waiter blocked on `lock().await` can proceed immediately.
        drop(guard);
        // Best-effort reclaim: if we hold the only remaining strong
        // reference to this per-Id lock, drop the map entry. `strong_count`
        // is racy (a new caller could clone the Arc between the check
        // and the remove), so we double-check inside `remove_if`; a lost
        // race just leaves the entry in place, which the next caller
        // will happily reuse.
        let key = id.to_string();
        drop(lock);
        SOURCE_FETCH_LOCKS.remove_if(&key, |_, v| Arc::strong_count(v) == 1);
        result
    }

    /// Reports whether the source's artifact is already present in the
    /// local cache backing `storage`.
    ///
    /// Used by transforms to short-circuit the per-node `prepare` task in
    /// the scheduler's fetch phase: if every input source reports cached,
    /// `prepare` would only re-confirm what we already know, so the
    /// scheduler can skip spawning the task entirely.
    ///
    /// Probes only the local cache \u2014 networked source caches are not
    /// consulted, because doing so would defeat the point (the goal is to
    /// avoid network IO when everything is already on disk).
    pub async fn is_cached(&self, storage: &Storage) -> SourceResult<bool> {
        let id = self.get_unique_id().await?;
        Ok(storage.has_local(&id).await?)
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
        let id = self.get_unique_id().await?;
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
        Storage::init(&Backend::new(local)).await.unwrap()
    }

    // Because Source is an arc_handle-generated trait, we build a small
    // hand-rolled impl for the singleflight test instead of using
    // mockall (which has trouble crossing the arc_handle boundary in
    // this codebase). The impl counts calls to `fetch` and returns a
    // synthetic artifact that lands in the local cache so the second
    // caller observes the fast path.
    struct CountingSource {
        id: Id,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SourceImpl for CountingSource {
        async fn get_unique_id(&self) -> SourceResult<Id> {
            Ok(self.id.clone())
        }
        async fn fetch(&self, _log: &Log, storage: &Storage) -> SourceResult<Artifact> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Simulate a slow fetch so peers pile up on the lock.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // Manufacture a minimal artifact and save it to the local
            // cache so the re-check under the lock succeeds for peers.
            use crate::storage::{Artifact as A, Config as ArtCfg, LayerOptions, MediaType};
            let mut writer = storage.safe_start_layer().await?;
            // Write a single zero byte so the layer has content.
            use tokio::io::AsyncWriteExt;
            writer.write_all(b"x").await.ok();
            writer.shutdown().await.ok();
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
        let source = Source::new(CountingSource {
            id: id.clone(),
            calls: calls.clone(),
        });

        // Spawn 10 concurrent cache() calls for the same source. All
        // ten should return the same artifact; only one should have
        // called `fetch`.
        let mut handles = Vec::new();
        for _ in 0..10 {
            let src = source.clone();
            let log = log.clone();
            let storage = storage.clone();
            handles.push(tokio::spawn(async move { src.cache(&log, &storage).await }));
        }
        for h in handles {
            h.await.expect("join").expect("cache");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "singleflight should collapse 10 concurrent cache() calls to a single fetch"
        );
    }
}
