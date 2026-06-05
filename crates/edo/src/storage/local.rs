use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::context::{Config, Element, FromElementNoContext};
use crate::storage::{Artifact, BackendImpl, Id, Layer, LayerOptions, StorageResult};
use crate::util::{Reader, Writer};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use snafu::{OptionExt, ResultExt, ensure};
use tokio::fs::{File, OpenOptions};
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
        let layer_dir = path.join("blobs/blake3");
        if !layer_dir.exists() {
            tokio::fs::create_dir_all(&layer_dir)
                .await
                .context(error::NewSnafu)?;
        }
        // Load the catalog once at construction; subsequent reads consult
        // the in-memory snapshot, and mutating methods keep both copies
        // in sync under the write lock.
        let catalog = Self::load_at(&catalog_file)?;
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
    fn load_at(path: &Path) -> StorageResult<Catalog> {
        if !path.exists() {
            return Ok(Catalog::default());
        }
        let mut reader = std::fs::File::open(path).context(error::ReadCatalogSnafu)?;
        let catalog: Catalog =
            serde_json::from_reader(&mut reader).context(error::DeserializeSnafu)?;
        Ok(catalog)
    }

    fn flush_at(path: &Path, catalog: &Catalog) -> StorageResult<()> {
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .context(error::WriteCatalogSnafu)?;
        serde_json::to_writer(&mut writer, catalog).context(error::SerializeSnafu)?;
        Ok(())
    }
}

#[async_trait]
impl BackendImpl for LocalBackend {
    async fn list(&self) -> StorageResult<BTreeSet<Id>> {
        Ok(self.catalog.read().catalog.list_all())
    }

    async fn has(&self, id: &Id) -> StorageResult<bool> {
        Ok(self.catalog.read().catalog.has(id))
    }

    async fn open(&self, id: &Id) -> StorageResult<Artifact> {
        let guard = self.catalog.read();
        let artifact = guard
            .catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?;
        Ok(artifact.clone())
    }

    async fn save(&self, artifact: &Artifact) -> StorageResult<()> {
        // Before we allow the save we should validate that all layers exist
        for layer in artifact.layers() {
            let blob_path = self.layer_dir.join(layer.digest().digest());
            ensure!(
                blob_path.exists(),
                error::LayerMissingSnafu {
                    digest: layer.digest().digest()
                }
            );
        }
        // Hold the write lock across mutate+flush so concurrent saves
        // cannot read a stale snapshot and clobber each other's writes.
        let mut guard = self.catalog.write();
        guard.catalog.add(artifact);
        Self::flush_at(&guard.path.clone(), &guard.catalog)?;
        Ok(())
    }

    async fn del(&self, id: &Id) -> StorageResult<()> {
        // Hold the write lock across the read-modify-write so concurrent
        // saves/dels cannot interleave and lose updates.
        let artifact = {
            let mut guard = self.catalog.write();
            if !guard.catalog.has(id) {
                return Ok(());
            }
            let artifact = guard
                .catalog
                .get(id)
                .context(error::NotFoundSnafu { id: id.clone() })?
                .clone();
            guard.catalog.del(id);
            Self::flush_at(&guard.path.clone(), &guard.catalog)?;
            artifact
        };
        for layer in artifact.layers() {
            let digest = layer.digest().digest();
            let blob_path = self.layer_dir.join(digest.clone());
            // Re-check the blob refcount under the read lock; another
            // concurrent save may have re-introduced it.
            let drop_blob = {
                let guard = self.catalog.read();
                guard.catalog.count(layer) <= 0 && blob_path.exists()
            };
            if drop_blob {
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
        let matching = self.catalog.read().catalog.matching(id);

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
        let path = {
            let mut guard = self.catalog.write();
            guard.catalog = Catalog::default();
            guard.path.clone()
        };
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .context(error::RemoveSnafu)?;
        }
        if self.layer_dir.exists() {
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
