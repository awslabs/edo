use std::collections::BTreeSet;
use std::fs::OpenOptions as StdOpenOptions;
use std::path::{Path, PathBuf};

use crate::context::{Addr, Config, FromNodeNoContext, Node};
use crate::non_configurable_no_context;
use crate::storage::{Artifact, BackendImpl, Id, Layer, LayerOptions, StorageResult};
use crate::util::{Reader, Writer};
use async_trait::async_trait;
use rustix::fs::{FlockOperation, flock};
use snafu::{IntoError, OptionExt, ResultExt, ensure};
use tokio::fs::{File, OpenOptions};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::catalog::Catalog;

/// Local filesystem storage backend.
///
/// Layers are stored as individual blobs under `blobs/blake3/<digest>` and
/// manifests are tracked in a JSON catalog file. The shared blob layout means
/// copy operations are metadata-only.
///
/// The on-disk catalog is mirrored by an in-memory [`Catalog`] snapshot
/// (see [`CatalogSlot`]) that is kept current by every mutating method.
/// Reads consult the snapshot directly, avoiding repeated JSON deserialization
/// of `catalog.json` on hot paths like the scheduler's fetch phase.
///
/// Concurrency model:
///
/// * Within a single process, the snapshot is guarded by a
///   [`tokio::sync::RwLock`] so the write path can hold the guard across
///   async filesystem operations (atomic catalog flush, blob deletion).
///   Mutating ops serialize with each other; reads run in parallel and
///   only block when a write is in progress.
/// * Across processes, mutating ops acquire an exclusive advisory
///   [`flock`] on `catalog.lock` (a sibling of `catalog.json`) before
///   reloading the on-disk catalog into the snapshot, applying the
///   mutation, and flushing back. This prevents the "last writer wins"
///   race where two `edo` processes each hold a stale in-memory copy
///   and clobber each other's catalog entries on save.
///
/// The flock is released when the file handle is dropped at the end of
/// each mutating call, so it is only held for the duration of a single
/// read-modify-write cycle, not the lifetime of the backend.
#[derive(Debug)]
pub struct LocalBackend {
    layer_dir: PathBuf,
    catalog: RwLock<CatalogSlot>,
}

/// Path + in-memory snapshot of the on-disk catalog.
///
/// The path is held alongside the snapshot so a single lock guard covers
/// both the file location and its decoded contents — preserving the
/// "lock held across read/modify/write" guarantee that mutating methods
/// rely on, while letting reads skip disk IO entirely.
///
/// `lock_path` names the sibling file used for the cross-process advisory
/// `flock`. Kept alongside `path` because the two are always locked and
/// flushed as a pair.
#[derive(Debug)]
struct CatalogSlot {
    path: PathBuf,
    lock_path: PathBuf,
    catalog: Catalog,
}

/// Guard holding an exclusive advisory `flock` on `catalog.lock`.
///
/// Acquired by [`LocalBackend::lock_catalog`] before any mutating catalog
/// operation; dropped (releasing the lock) when the operation returns.
/// Constructed via `spawn_blocking` because `flock(LOCK_EX)` may block
/// arbitrarily long waiting for a peer, and we must not stall the tokio
/// runtime while doing so.
#[derive(Debug)]
struct CatalogLock {
    // Hold the file open until drop so the kernel keeps the fd-scoped
    // advisory lock alive for the whole read-modify-write cycle.
    _file: std::fs::File,
}

#[async_trait]
impl FromNodeNoContext for LocalBackend {
    type Error = crate::storage::StorageError;

    async fn from_node(
        _addr: &Addr,
        node: &Node,
        _config: &Config,
    ) -> std::result::Result<Self, Self::Error> {
        node.validate_keys(&["path"])?;
        let path = node
            .get("path")
            .and_then(|x| x.as_string())
            .context(error::PathNotSpecifiedSnafu)?;
        Self::new_(path).await
    }
}

non_configurable_no_context!(LocalBackend, crate::storage::StorageError);

unsafe impl Send for LocalBackend {}
unsafe impl Sync for LocalBackend {}

impl LocalBackend {
    async fn new_(path: impl AsRef<Path>) -> StorageResult<Self> {
        let path = path.as_ref();
        trace!(
            subsystem = "storage",
            component = "local",
            path = %path.display(),
            "creating or loading local storage"
        );
        if !path.exists() {
            tokio::fs::create_dir_all(path)
                .await
                .context(error::NewSnafu)?;
        }
        let catalog_file = path.join("catalog.json");
        let lock_file = path.join("catalog.lock");
        let layer_dir = path.join("blobs/blake3");
        if !layer_dir.exists() {
            tokio::fs::create_dir_all(&layer_dir)
                .await
                .context(error::NewSnafu)?;
        }
        // Prime the in-memory snapshot from disk. Read paths consult this
        // copy without touching the filesystem; write paths reload the
        // catalog under an OS-level flock before mutating, so a stale
        // snapshot here is harmless — it will be refreshed on first write.
        let catalog = Self::load_at(&catalog_file).await?;
        Ok(Self {
            layer_dir,
            catalog: RwLock::new(CatalogSlot {
                path: catalog_file,
                lock_path: lock_file,
                catalog,
            }),
        })
    }
}

impl LocalBackend {
    async fn load_at(path: &Path) -> StorageResult<Catalog> {
        if !tokio::fs::try_exists(path)
            .await
            .context(error::ReadCatalogSnafu)?
        {
            return Ok(Catalog::default());
        }
        let bytes = tokio::fs::read(path)
            .await
            .context(error::ReadCatalogSnafu)?;
        serde_json::from_slice(&bytes)
            .context(error::DeserializeSnafu)
            .map_err(Into::into)
    }

    /// Acquire the cross-process advisory lock guarding `catalog.json`.
    ///
    /// Opens (creating if necessary) `catalog.lock` and calls `flock` with
    /// `LOCK_EX`, blocking until any other process holding the lock
    /// releases it. The lock lives for as long as the returned
    /// [`CatalogLock`] is alive — it must be held for the entire
    /// read-modify-write cycle so a peer cannot slip a stale write in
    /// between our reload and flush.
    ///
    /// Runs on a blocking task because `flock(LOCK_EX)` can wait
    /// arbitrarily long. Advisory locks are Unix-only in this
    /// implementation; on non-Unix the compile will fail early which is
    /// preferable to a silent single-process fallback that would
    /// reintroduce the race.
    async fn lock_catalog(lock_path: &Path) -> StorageResult<CatalogLock> {
        let lock_path = lock_path.to_path_buf();
        let join = tokio::task::spawn_blocking(move || -> StorageResult<CatalogLock> {
            let file = StdOpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .context(error::LockSnafu)?;
            flock(&file, FlockOperation::LockExclusive).context(error::FlockSnafu)?;
            Ok(CatalogLock { _file: file })
        })
        .await;
        match join {
            Ok(inner) => inner,
            Err(e) => Err(error::LockSnafu.into_error(std::io::Error::other(e)).into()),
        }
    }

    /// Atomically write the catalog to disk by serializing into a sibling
    /// temp file and renaming over the target. `rename(2)` is atomic
    /// within a filesystem, so a concurrent reader either sees the old
    /// file or the new file — never an empty/partial one.
    async fn flush_at(path: &Path, catalog: &Catalog) -> StorageResult<()> {
        let bytes = serde_json::to_vec(catalog).context(error::SerializeSnafu)?;
        let tmp = path.with_extension("json.tmp");
        // Use `OpenOptions` with `create(true).truncate(true)` so a
        // leftover tmp file from a previous crash is overwritten.
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await
            .context(error::WriteCatalogSnafu)?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes)
            .await
            .context(error::WriteCatalogSnafu)?;
        file.sync_all().await.context(error::WriteCatalogSnafu)?;
        drop(file);
        tokio::fs::rename(&tmp, path)
            .await
            .context(error::WriteCatalogSnafu)?;
        Ok(())
    }
}

#[async_trait]
impl BackendImpl for LocalBackend {
    async fn list(&self) -> StorageResult<BTreeSet<Id>> {
        Ok(self.catalog.read().await.catalog.list_all())
    }

    async fn has(&self, id: &Id) -> StorageResult<bool> {
        Ok(self.catalog.read().await.catalog.has(id))
    }

    async fn open(&self, id: &Id) -> StorageResult<Artifact> {
        let guard = self.catalog.read().await;
        let artifact = guard
            .catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?;
        Ok(artifact.clone())
    }

    async fn save(&self, artifact: &Artifact) -> StorageResult<()> {
        // Hold the write lock across precondition check, catalog mutation,
        // and on-disk flush so concurrent saves cannot race each other and
        // a concurrent `del` cannot remove a blob between our check and
        // our register.
        let mut guard = self.catalog.write().await;
        // Take the cross-process flock and re-read the on-disk catalog
        // before mutating so a peer's writes since our snapshot last
        // synced are preserved. Without this, two processes each holding
        // a stale in-memory copy would overwrite each other on flush.
        let _lock = Self::lock_catalog(&guard.lock_path).await?;
        guard.catalog = Self::load_at(&guard.path).await?;
        for layer in artifact.layers() {
            let blob_path = self.layer_dir.join(layer.digest().digest());
            ensure!(
                tokio::fs::try_exists(&blob_path)
                    .await
                    .context(error::ReadSnafu)?,
                error::LayerMissingSnafu {
                    digest: layer.digest().digest()
                }
            );
        }
        guard.catalog.add(artifact);
        let path = guard.path.clone();
        Self::flush_at(&path, &guard.catalog).await?;
        Ok(())
    }

    async fn del(&self, id: &Id) -> StorageResult<()> {
        // Hold the write lock across the entire delete: mutate the
        // catalog, flush it, then remove blob files. Holding the lock
        // until the blob files are gone closes the TOCTOU window where
        // a racing `save` could observe the blob via `try_exists`,
        // proceed past its precondition, and end up registering a
        // manifest pointing at a digest whose file we are about to
        // unlink.
        let mut guard = self.catalog.write().await;
        // Same rationale as `save`: reload under the cross-process flock
        // so we do not delete against a stale snapshot and lose peer
        // updates when we flush.
        let _lock = Self::lock_catalog(&guard.lock_path).await?;
        guard.catalog = Self::load_at(&guard.path).await?;
        if !guard.catalog.has(id) {
            return Ok(());
        }
        let artifact = guard
            .catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?
            .clone();
        guard.catalog.del(id);
        let path = guard.path.clone();
        Self::flush_at(&path, &guard.catalog).await?;
        for layer in artifact.layers() {
            if guard.catalog.count(layer) > 0 {
                continue;
            }
            let digest = layer.digest().digest();
            let blob_path = self.layer_dir.join(&digest);
            if tokio::fs::try_exists(&blob_path)
                .await
                .context(error::RemoveSnafu)?
            {
                tokio::fs::remove_file(&blob_path)
                    .await
                    .context(error::RemoveSnafu)?;
            }
        }
        Ok(())
    }

    async fn copy(&self, from: &Id, to: &Id) -> StorageResult<()> {
        // The best part about a copy operation with the shared blob store is that
        // we don't have to copy any actual data :D only the manifest links which
        // is doable by simply opening the artifact manifest, modifying the id and saving
        // the result
        let mut artifact = self.open(from).await?;
        *artifact.config_mut().id_mut() = to.clone();
        self.save(&artifact).await?;
        Ok(())
    }

    async fn prune(&self, id: &Id) -> StorageResult<()> {
        trace!(
            subsystem = "storage",
            component = "local",
            op = "prune",
            prefix = %id.prefix(),
            "pruning all artifacts that do not match prefix"
        );
        // Refresh from disk under the cross-process flock before
        // choosing what to prune, otherwise a stale snapshot could
        // (a) miss entries a peer just added, leaving them to accumulate,
        // or (b) list entries a peer just deleted, whose per-entry
        // `del` call would then be a no-op but still spin the reload
        // cycle. Do the reload under a fresh short-lived flock so we
        // don't hold it across the per-entry `del` calls (which each
        // take their own).
        {
            let mut guard = self.catalog.write().await;
            let _lock = Self::lock_catalog(&guard.lock_path).await?;
            guard.catalog = Self::load_at(&guard.path).await?;
        }
        let matching = self.catalog.read().await.catalog.matching(id);

        for entry in matching {
            if entry == *id {
                continue;
            }
            debug!(
                subsystem = "storage",
                component = "local",
                op = "prune",
                id = %entry,
                "pruning artifact"
            );
            self.del(&entry).await?;
        }
        Ok(())
    }

    async fn prune_all(&self) -> StorageResult<()> {
        // Take the write lock first so reads see an empty snapshot the
        // instant the on-disk file disappears. The lock is held across
        // the (cheap) filesystem removals; no other thread can observe
        // the half-removed state.
        let mut guard = self.catalog.write().await;
        // Also take the cross-process flock so a peer save/del that
        // started before our write lands on top of our just-emptied
        // catalog and resurrects state we intended to wipe.
        let lock = Self::lock_catalog(&guard.lock_path).await?;
        guard.catalog = Catalog::default();
        let path = guard.path.clone();
        let lock_path = guard.lock_path.clone();
        if tokio::fs::try_exists(&path)
            .await
            .context(error::RemoveSnafu)?
        {
            tokio::fs::remove_file(&path)
                .await
                .context(error::RemoveSnafu)?;
        }
        if tokio::fs::try_exists(&self.layer_dir)
            .await
            .context(error::RemoveSnafu)?
        {
            tokio::fs::remove_dir_all(&self.layer_dir)
                .await
                .context(error::RemoveSnafu)?;
        }
        // Release the flock before unlinking the lock file so the fd we
        // hold is closed cleanly; another process that was blocked in
        // `flock` will then create a fresh lock file on its next attempt.
        drop(lock);
        if tokio::fs::try_exists(&lock_path)
            .await
            .context(error::RemoveSnafu)?
        {
            tokio::fs::remove_file(&lock_path)
                .await
                .context(error::RemoveSnafu)?;
        }
        Ok(())
    }

    async fn read(&self, layer: &Layer) -> StorageResult<Reader> {
        // A Read is a pretty simple operation, we just want to load the correct blob file
        let blob_digest = layer.digest().digest();
        let blob_file = self.layer_dir.join(blob_digest);
        Ok(Reader::new(
            File::open(&blob_file).await.context(error::ReadSnafu)?,
        ))
    }

    async fn start_layer(&self) -> StorageResult<Writer> {
        // A new layer starts its life as a temporary file
        let tmp_name = format!("{}.tmp", Uuid::now_v7());
        let file_path = self.layer_dir.join(tmp_name.clone());
        Ok(Writer::new(
            tmp_name.clone(),
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&file_path)
                .await
                .context(error::CreateSnafu)?,
        ))
    }

    async fn finish_layer(&self, writer: &Writer, options: &LayerOptions) -> StorageResult<Layer> {
        // The writer will contain the temporary file name to use
        let tmp_path = self.layer_dir.join(writer.target());
        // Now we want to calculate the digest
        let digest = writer.finish().await;
        let target_path = self.layer_dir.join(digest.clone());
        let layer = options.create(digest, writer.size());

        // Copy the layer to the appropriate place
        if tmp_path != target_path {
            tokio::fs::copy(&tmp_path, &target_path)
                .await
                .context(error::CopySnafu)?;
            tokio::fs::remove_file(&tmp_path)
                .await
                .context(error::RemoveSnafu)?;
        }
        Ok(layer)
    }

    async fn has_blob(&self, digest: &str) -> StorageResult<bool> {
        // Treat the catalog as a hint, not an authority: confirm the
        // file actually exists on disk before reporting `true`. This
        // closes the gap where the catalog and filesystem disagree
        // (out-of-band corruption, partial backup restore, etc.) and
        // a false positive would have steered the caller into the
        // digest-verification short-circuit.
        if !self.catalog.read().await.catalog.has_blob(digest) {
            return Ok(false);
        }
        let path = self.layer_dir.join(digest);
        Ok(tokio::fs::try_exists(&path)
            .await
            .context(error::ReadSnafu)?)
    }

    async fn blob_size(&self, digest: &str) -> StorageResult<Option<u64>> {
        let path = self.layer_dir.join(digest);
        match tokio::fs::metadata(&path).await {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(error::ReadSnafu.into_error(e).into()),
        }
    }
}

pub(crate) mod error {
    use snafu::Snafu;

    use crate::storage::StorageError;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub(crate)))]
    pub(crate) enum Error {
        #[snafu(display("failed to deserialize manifest: {source}"))]
        Deserialize { source: serde_json::Error },
        #[snafu(display("failed to copy blob: {source}"))]
        Copy { source: std::io::Error },
        #[snafu(display("failed to create temporary file for new layer: {source}"))]
        Create { source: std::io::Error },
        #[snafu(display("cannot save an artifact that is missing a layer with digest '{digest}'"))]
        LayerMissing { digest: String },
        #[snafu(display("failed to create new local storage backend: {source}"))]
        New { source: std::io::Error },
        #[snafu(display("storage backend does not contain an artifact with id: {id}"))]
        NotFound { id: crate::storage::Id },
        #[snafu(display("configuration for a local storage requires a 'path' field"))]
        PathNotSpecified,
        #[snafu(display("failed to open layer for reading: {source}"))]
        Read { source: std::io::Error },
        #[snafu(display("failed to read catalog: {source}"))]
        ReadCatalog { source: std::io::Error },
        #[snafu(display("failed to remove locally stored blob: {source}"))]
        Remove { source: std::io::Error },
        #[snafu(display("failed to serialize manifest: {source}"))]
        Serialize { source: serde_json::Error },
        #[snafu(display("failed to write catalog: {source}"))]
        WriteCatalog { source: std::io::Error },
        #[snafu(display("failed to open catalog lock file: {source}"))]
        Lock { source: std::io::Error },
        #[snafu(display("failed to acquire cross-process catalog lock: {source}"))]
        Flock { source: rustix::io::Errno },
    }

    impl From<Error> for StorageError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Config as ArtifactConfig, MediaType};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, LocalBackend) {
        let dir = TempDir::new().expect("tempdir");
        let backend = LocalBackend::new_(dir.path()).await.expect("new local");
        (dir, backend)
    }

    fn artifact(name: &str, digest: &str) -> Artifact {
        let id = Id::builder()
            .name(name.to_string())
            .digest(digest.to_string())
            .build();
        Artifact::builder()
            .media_type(MediaType::Manifest)
            .config(ArtifactConfig::builder().id(id).build())
            .build()
    }

    #[tokio::test]
    async fn save_then_open_round_trips() {
        let (_dir, b) = setup().await;
        let a = artifact("foo", "deadbeef");
        b.save(&a).await.expect("save");
        let opened = b.open(a.config().id()).await.expect("open");
        assert_eq!(opened.config().id(), a.config().id());
    }

    #[tokio::test]
    async fn flush_is_atomic_no_orphan_tmp() {
        // After a save, the on-disk layout should be the catalog file and
        // (for an empty artifact) no orphan tmp files in the storage root.
        let (dir, b) = setup().await;
        let a = artifact("foo", "deadbeef");
        b.save(&a).await.expect("save");
        let entries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        // We allow `catalog.json` and `blobs`, but `catalog.json.tmp` must
        // not survive a successful flush.
        assert!(
            !entries.iter().any(|n| n.ends_with(".tmp")),
            "no tmp files after flush: {entries:?}"
        );
    }

    #[tokio::test]
    async fn save_persists_across_reload() {
        let dir = TempDir::new().expect("tempdir");
        {
            let b = LocalBackend::new_(dir.path()).await.expect("new local");
            b.save(&artifact("foo", "111")).await.expect("save");
        }
        // Re-open the same directory and confirm the manifest is loaded.
        let b = LocalBackend::new_(dir.path()).await.expect("reopen");
        let id = artifact("foo", "111").config().id().clone();
        assert!(b.has(&id).await.expect("has"));
    }

    #[tokio::test]
    async fn concurrent_backends_do_not_clobber_catalog() {
        // Two `LocalBackend` instances pointing at the same storage
        // directory each save a distinct artifact after both have
        // primed their in-memory snapshot from an empty catalog.
        // Without a cross-process reload under an OS-level lock, the
        // second `save` would flush its stale (empty + own artifact)
        // snapshot on top of the first, losing the first artifact.
        // With the flock + reload wired in, both survive.
        let dir = TempDir::new().expect("tempdir");
        let a = LocalBackend::new_(dir.path()).await.expect("open a");
        let b = LocalBackend::new_(dir.path()).await.expect("open b");

        let id_a = artifact("alpha", "aaa").config().id().clone();
        let id_b = artifact("beta", "bbb").config().id().clone();

        a.save(&artifact("alpha", "aaa")).await.expect("save a");
        b.save(&artifact("beta", "bbb")).await.expect("save b");

        // Re-open cleanly and confirm both manifests are present on disk.
        let fresh = LocalBackend::new_(dir.path()).await.expect("reopen");
        assert!(fresh.has(&id_a).await.expect("has a"), "alpha survived");
        assert!(fresh.has(&id_b).await.expect("has b"), "beta survived");
    }
}
