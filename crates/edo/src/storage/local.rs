use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::context::{Config, Element, FromElementNoContext};
use crate::storage::{Artifact, BackendImpl, Id, Layer, LayerOptions, StorageResult};
use crate::util::{Reader, Writer};
use async_trait::async_trait;
use serde_json::json;
use snafu::{IntoError, OptionExt, ResultExt, ensure};
use tokio::fs::{File, OpenOptions};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::catalog::Catalog;

/// Local filesystem storage backend.
///
/// Layers are stored as individual blobs under `blobs/sha256/<digest>` and
/// manifests are tracked in a JSON catalog file. The shared blob layout means
/// copy operations are metadata-only.
///
/// The on-disk catalog is mirrored by an in-memory [`Catalog`] snapshot
/// (see [`CatalogSlot`]) that is kept current by every mutating method.
/// Reads consult the snapshot directly, avoiding repeated JSON deserialization
/// of `catalog.json` on hot paths like the scheduler's fetch phase.
///
/// Concurrency model: the snapshot is guarded by a [`tokio::sync::RwLock`]
/// so the write path can hold the guard across async filesystem operations
/// (atomic catalog flush, blob deletion). Mutating ops thus serialize with
/// each other; reads run in parallel with each other and only block when a
/// write is in progress.
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
#[derive(Debug)]
struct CatalogSlot {
    path: PathBuf,
    catalog: Catalog,
}

#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct LocalBackendOptions {
    path: PathBuf,
}

#[async_trait]
impl FromElementNoContext for LocalBackend {
    type Error = crate::storage::StorageError;

    async fn new(element: &Element, _config: &Config) -> std::result::Result<Self, Self::Error> {
        let options: LocalBackendOptions =
            serde_json::from_value(json!(&element.config)).context(error::ConfigSnafu {
                addr: element.addr.clone(),
            })?;
        Self::new_(&options.path).await
    }
}

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
        let layer_dir = path.join("blobs/sha256");
        if !layer_dir.exists() {
            tokio::fs::create_dir_all(&layer_dir)
                .await
                .context(error::NewSnafu)?;
        }
        // Load the catalog once at construction; subsequent reads consult
        // the in-memory snapshot, and mutating methods keep both copies
        // in sync under the write lock.
        let catalog = Self::load_at(&catalog_file).await?;
        Ok(Self {
            layer_dir,
            catalog: RwLock::new(CatalogSlot {
                path: catalog_file,
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
        // Snapshot the prefix listing under the read lock; `del` takes
        // the write lock per-entry so we cannot hold our guard across it.
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
        guard.catalog = Catalog::default();
        let path = guard.path.clone();
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

    use crate::{context::Addr, storage::StorageError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub(crate)))]
    pub(crate) enum Error {
        #[snafu(display("failed to deserialize manifest: {source}"))]
        Deserialize { source: serde_json::Error },
        #[snafu(display("element {addr} has invalid configuration: {source}"))]
        Config {
            addr: Addr,
            source: serde_json::Error,
        },
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
        // The reload path goes through `serde_json::from_slice` for the
        // catalog, which deserializes `Id`s via `FromStr`. That parser
        // requires a 64-hex SHA256 digest, so this test cannot use a
        // short placeholder digest.
        const D: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let dir = TempDir::new().expect("tempdir");
        {
            let b = LocalBackend::new_(dir.path()).await.expect("new local");
            b.save(&artifact("foo", D)).await.expect("save");
        }
        // Re-open the same directory and confirm the manifest is loaded.
        let b = LocalBackend::new_(dir.path()).await.expect("reopen");
        let id = artifact("foo", D).config().id().clone();
        assert!(b.has(&id).await.expect("has"));
    }
}
