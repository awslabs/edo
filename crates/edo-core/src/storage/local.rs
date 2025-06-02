use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::context::{Addr, Config, FromNodeNoContext, Node};
use crate::non_configurable_no_context;
use crate::storage::{Artifact, BackendImpl, Id, Layer, LayerBuilder, MediaType, StorageResult};
use crate::util::{Reader, Writer};
use async_trait::async_trait;
use ocilot::models::Platform;
use parking_lot::RwLock;
use snafu::{OptionExt, ResultExt, ensure};
use tokio::fs::{File, OpenOptions};
use uuid::Uuid;

use super::catalog::Catalog;

/// Implements a local storage backend
/// all layers are stored in blobs/blake3/<digest>...
/// all manifests are stored in a redb database file
#[derive(Debug)]
pub struct LocalBackend {
    layer_dir: PathBuf,
    catalog_file: RwLock<PathBuf>,
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
            section = "storage",
            component = "backend",
            variant = "local",
            "creating or loading local storage at {}",
            path.display()
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
        Ok(Self {
            layer_dir,
            catalog_file: RwLock::new(catalog_file),
        })
    }
}

impl LocalBackend {
    fn load(&self) -> StorageResult<Catalog> {
        // Acquire a lock on the catalog
        let lock = self.catalog_file.read();
        if !lock.exists() {
            return Ok(Catalog::default());
        }
        let mut reader = std::fs::File::open(lock.as_path()).context(error::ReadCatalogSnafu)?;
        let catalog: Catalog =
            serde_json::from_reader(&mut reader).context(error::DeserializeSnafu)?;
        Ok(catalog)
    }

    fn flush(&self, catalog: &Catalog) -> StorageResult<()> {
        // Acquire a write lock
        let lock = self.catalog_file.write();
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(lock.as_path())
            .context(error::WriteCatalogSnafu)?;
        serde_json::to_writer(&mut writer, catalog).context(error::SerializeSnafu)?;
        Ok(())
    }
}

#[async_trait]
impl BackendImpl for LocalBackend {
    async fn list(&self) -> StorageResult<BTreeSet<Id>> {
        let catalog = self.load()?;
        Ok(catalog.list_all())
    }

    async fn has(&self, id: &Id) -> StorageResult<bool> {
        let catalog = self.load()?;
        Ok(catalog.has(id))
    }

    async fn open(&self, id: &Id) -> StorageResult<Artifact> {
        let catalog = self.load()?;
        let artifact = catalog
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
        // Now we can write everything into the catalog
        let mut catalog = self.load()?;
        catalog.add(artifact);
        self.flush(&catalog)?;
        Ok(())
    }

    async fn del(&self, id: &Id) -> StorageResult<()> {
        if !self.has(id).await? {
            // Do nothing if we don't have this id
            return Ok(());
        }
        // First load the existing metadata
        let mut catalog = self.load()?;
        let artifact = catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?
            .clone();
        catalog.del(id);
        self.flush(&catalog)?;
        for layer in artifact.layers() {
            let digest = layer.digest().digest();
            let blob_path = self.layer_dir.join(digest.clone());
            if catalog.count(layer) <= 0 && blob_path.exists() {
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
            section = "storage",
            component = "backend",
            variant = "local",
            "prunning all artifacts that do not match prefix: {}",
            id.prefix()
        );
        // To prune historical artifacts we want to load our catalog for the id prefix
        let catalog = self.load()?;

        for entry in catalog.matching(id) {
            if entry == *id {
                continue;
            }
            info!(
                section = "storage",
                component = "backend",
                variant = "local",
                "prunning artifact {entry}"
            );
            self.del(&entry).await?;
        }
        Ok(())
    }

    async fn prune_all(&self) -> StorageResult<()> {
        let lock = self.catalog_file.write();
        tokio::fs::remove_file(lock.as_path())
            .await
            .context(error::RemoveSnafu)?;
        tokio::fs::remove_dir_all(&self.layer_dir)
            .await
            .context(error::RemoveSnafu)?;
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

    async fn finish_layer(
        &self,
        media_type: &MediaType,
        platform: Option<Platform>,
        writer: &Writer,
    ) -> StorageResult<Layer> {
        // The writer will contain the temporary file name to use
        let tmp_path = self.layer_dir.join(writer.target());
        // Now we want to calculate the digest
        let digest = writer.finish().await;
        let target_path = self.layer_dir.join(digest.clone());
        let layer = LayerBuilder::default()
            .digest(digest.clone())
            .media_type(media_type.clone())
            .size(writer.size())
            .platform(platform)
            .build()
            .context(error::LayerSnafu)?;

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

    use crate::storage::{LayerBuilderError, StorageError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub(crate)))]
    pub(crate) enum Error {
        #[snafu(display("failed to deserialize manifest: {source}"))]
        Deserialize { source: serde_json::Error },
        #[snafu(display("failed to copy blob: {source}"))]
        Copy { source: std::io::Error },
        #[snafu(display("failed to create temporary file for new layer: {source}"))]
        Create { source: std::io::Error },
        #[snafu(display("failed to make a layer: {source}"))]
        Layer { source: LayerBuilderError },
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
