use async_trait::async_trait;
use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_s3::{
    client::Client,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use edo::{
    context::{Config, Element, FromElementNoContext},
    storage::{Artifact, BackendImpl, Id, Layer, LayerOptions, StorageResult},
    util::{Reader, Writer},
};
use snafu::{IntoError, OptionExt, ResultExt};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::{fs::OpenOptions, io::AsyncReadExt};
use uuid::Uuid;

use edo::storage::Catalog;

mod error;
mod reader;

type Result<T> = std::result::Result<T, error::Error>;
const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10mb

/// The user configurable options for the s3 backend
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct S3BackendOptions {
    bucket: String,
    prefix: Option<PathBuf>,
}

/// An S3-backed storage backend for artifact caching and retrieval.
pub struct S3Backend {
    client: Arc<Client>,
    bucket: String,
    prefix: Option<PathBuf>,
    catalog_key: String,
}

unsafe impl Send for S3Backend {}
unsafe impl Sync for S3Backend {}

#[async_trait]
impl FromElementNoContext for S3Backend {
    type Error = edo::storage::StorageError;

    async fn new(element: &Element, _config: &Config) -> std::result::Result<Self, Self::Error> {
        let options: S3BackendOptions = element.get()?;
        Self::new_(
            &aws_config::load_defaults(BehaviorVersion::latest()).await,
            &options.bucket,
            options.prefix,
        )
        .await
    }
}

impl S3Backend {
    /// Creates a new S3 backend with the given SDK configuration, bucket, and optional key prefix.
    pub async fn new_(
        sdk_config: &SdkConfig,
        bucket: &str,
        prefix: Option<PathBuf>,
    ) -> StorageResult<Self> {
        info!(
            subsystem = "storage",
            component = "s3",
            op = "register",
            bucket = %bucket,
            prefix = %prefix.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            "creating or loading s3 cache"
        );
        let client = Arc::new(Client::new(sdk_config));
        let catalog_key = if let Some(prefix) = prefix.as_ref() {
            format!("{}/catalog.json", prefix.display())
        } else {
            "catalog.json".to_string()
        };

        Ok(Self {
            client: client.clone(),
            bucket: bucket.into(),
            prefix,
            catalog_key,
        })
    }

    /// Returns the S3 key prefix for blob storage.
    pub fn blob_key(&self) -> PathBuf {
        if let Some(prefix) = self.prefix.as_ref() {
            prefix.join("blobs/sha256")
        } else {
            PathBuf::from("blobs/sha256")
        }
    }

    /// Loads the artifact catalog from S3, returning a default catalog if none exists.
    pub async fn load(&self) -> StorageResult<Catalog> {
        // check if the catalog exists
        if self
            .client
            .head_object()
            .bucket(self.bucket.clone())
            .key(self.catalog_key.clone())
            .send()
            .await
            .is_err()
        {
            return Ok(Catalog::default());
        }
        // you can always read the current state of the catalog
        let response = self
            .client
            .get_object()
            .bucket(self.bucket.clone())
            .key(self.catalog_key.clone())
            .send()
            .await
            .context(error::GetSnafu)?;
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|e| error::ReadBodySnafu.into_error(Box::new(e)))?;
        serde_json::from_slice::<Catalog>(bytes.to_vec().as_slice())
            .context(error::DeserializeSnafu)
            .map_err(Into::into)
    }

    /// Polls until the catalog lock object is gone.
    ///
    /// S3 has no native compare-and-swap on object creation, so concurrent
    /// writers cannot use this to *guarantee* serialized access; the lock
    /// object is a best-effort hint to back off when another writer is
    /// known to be in progress. Returns `LockTimeout` once the retry
    /// budget is exhausted so the caller does not silently trample a
    /// peer's in-flight write — the previous behaviour of "warn and
    /// proceed anyway" was racy and could lose updates.
    pub async fn wait_for_lock(&self) -> StorageResult<()> {
        const MAX_ATTEMPTS: u32 = 5;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        for attempt in 1..=MAX_ATTEMPTS {
            interval.tick().await;
            if self
                .client
                .head_object()
                .bucket(self.bucket.clone())
                .key(format!("{}.lock", self.catalog_key))
                .send()
                .await
                .is_err()
            {
                return Ok(());
            }
            if attempt == MAX_ATTEMPTS {
                error!(
                    subsystem = "storage",
                    component = "s3",
                    catalog_key = %self.catalog_key,
                    "lock object {}.lock did not clear after {MAX_ATTEMPTS} attempts; failing",
                    self.catalog_key,
                );
                return Err(error::LockTimeoutSnafu {
                    key: format!("{}.lock", self.catalog_key),
                }
                .build()
                .into());
            }
        }
        Ok(())
    }

    /// Writes the catalog to S3, using a lock file to coordinate concurrent access.
    pub async fn flush(&self, catalog: &Catalog) -> StorageResult<()> {
        self.wait_for_lock().await?;
        // First we create a lock file to signal any one else that we are writing
        self.client
            .put_object()
            .bucket(self.bucket.clone())
            .key(format!("{}.lock", self.catalog_key))
            .body(ByteStream::from_static(b"lock"))
            .send()
            .await
            .context(error::PutSnafu)?;
        let bytes = serde_json::to_vec(catalog).context(error::SerializeSnafu)?;
        let result = self
            .client
            .put_object()
            .bucket(self.bucket.clone())
            .key(self.catalog_key.clone())
            .body(ByteStream::from(bytes))
            .send()
            .await
            .context(error::PutSnafu);
        // Regardless if the above put failed or succeeded we need to clear the lockfile
        self.client
            .delete_object()
            .bucket(self.bucket.clone())
            .key(format!("{}.lock", self.catalog_key))
            .send()
            .await
            .context(error::DeleteSnafu)?;
        let _ = result?;
        Ok(())
    }
}

#[async_trait]
impl BackendImpl for S3Backend {
    async fn list(&self) -> StorageResult<BTreeSet<Id>> {
        let catalog = self.load().await?;
        Ok(catalog.list_all())
    }

    async fn has(&self, id: &Id) -> StorageResult<bool> {
        let catalog = self.load().await?;
        Ok(catalog.has(id))
    }

    async fn open(&self, id: &Id) -> StorageResult<Artifact> {
        let catalog = self.load().await?;
        let artifact = catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?;
        Ok(artifact.clone())
    }

    async fn save(&self, artifact: &Artifact) -> StorageResult<()> {
        let mut catalog = self.load().await?;
        catalog.add(artifact);
        self.flush(&catalog).await?;
        Ok(())
    }

    async fn del(&self, id: &Id) -> StorageResult<()> {
        if !self.has(id).await? {
            // Do nothing if we don't have this id
            return Ok(());
        }
        // First load the existing metadata
        let mut catalog = self.load().await?;
        let artifact = catalog
            .get(id)
            .context(error::NotFoundSnafu { id: id.clone() })?
            .clone();
        catalog.del(id);
        self.flush(&catalog).await?;
        for layer in artifact.layers() {
            let digest = layer.digest().digest();
            let key = self.blob_key().join(digest);
            if catalog.count(layer) <= 0 {
                self.client
                    .delete_object()
                    .bucket(self.bucket.clone())
                    .key(key.to_str().unwrap())
                    .send()
                    .await
                    .context(error::DeleteSnafu)?;
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
            component = "s3",
            op = "prune",
            prefix = %id.prefix(),
            "pruning all artifacts that do not match prefix"
        );
        // To prune historical artifacts we want to load our catalog for the id prefix
        let catalog = self.load().await?;

        for entry in catalog.matching(id) {
            if entry == *id {
                continue;
            }
            debug!(
                subsystem = "storage",
                component = "s3",
                op = "prune",
                id = %entry,
                "pruning artifact"
            );
            self.del(&entry).await?;
        }
        Ok(())
    }

    async fn prune_all(&self) -> StorageResult<()> {
        let result = error::PruneAllSnafu {}.fail();
        result.map_err(|e| e.into())
    }

    async fn read(&self, layer: &Layer) -> StorageResult<Reader> {
        // A Read is a pretty simple operation, we just want to load the correct blob file
        let blob_digest = layer.digest().digest();
        let blob_file = self.blob_key().join(blob_digest);
        Ok(Reader::new(
            reader::ObjectReader::new(
                self.client.clone(),
                self.bucket.as_str(),
                blob_file.to_str().unwrap(),
            )
            .await?,
        ))
    }

    async fn start_layer(&self) -> StorageResult<Writer> {
        // A new layer starts its life as a temporary file
        let tmp_name = format!("{}.tmp", Uuid::now_v7());
        // Due to issues wrapping a multipart upload we actually write to a local file then upload it all
        // when layer is finished
        let tmp_file_path = std::env::temp_dir().join(tmp_name.clone());
        Ok(Writer::new(
            tmp_file_path.to_string_lossy().to_string(),
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_file_path)
                .await
                .context(error::TempSnafu)?,
        ))
    }

    async fn finish_layer(&self, writer: &Writer, options: &LayerOptions) -> StorageResult<Layer> {
        // The writer will contain the temporary file name to use
        let tmp_path = std::env::temp_dir().join(writer.target());
        // Now we want to calculate the digest
        let digest = writer.finish().await;
        let target_path = self.blob_key().join(digest.clone());
        let layer = options.create(digest, writer.size());

        let mut file = tokio::fs::File::open(&tmp_path)
            .await
            .context(error::TempSnafu)?;
        let file_size = file.metadata().await.context(error::TempSnafu)?.len();
        if file_size > CHUNK_SIZE as u64 {
            // The file is greater than 5m so we should do a multipart upload
            let mut parts = Vec::new();
            let response = self
                .client
                .create_multipart_upload()
                .bucket(self.bucket.clone())
                .key(target_path.to_str().unwrap())
                .send()
                .await
                .context(error::StartSnafu)?;
            let upload_id = response.upload_id().unwrap();
            let mut pos = 0usize;
            while pos < file_size as usize {
                let remaining = file_size as usize - pos;
                let chunk_size: usize = if remaining > CHUNK_SIZE {
                    CHUNK_SIZE
                } else {
                    remaining
                };
                // Read the chunk
                let mut buffer = vec![0; chunk_size];
                file.read_exact(buffer.as_mut_slice())
                    .await
                    .context(error::TempSnafu)?;
                let part_response = self
                    .client
                    .upload_part()
                    .bucket(self.bucket.clone())
                    .key(target_path.to_str().unwrap())
                    .body(ByteStream::from(buffer))
                    .upload_id(upload_id)
                    .part_number(parts.len() as i32 + 1)
                    .send()
                    .await
                    .context(error::PartSnafu)?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(parts.len() as i32 + 1)
                        .set_e_tag(part_response.e_tag)
                        .build(),
                );
                pos += chunk_size;
            }
            self.client
                .complete_multipart_upload()
                .bucket(self.bucket.clone())
                .key(target_path.to_str().unwrap())
                .upload_id(upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .context(error::FinishSnafu)?;
        } else {
            // The file is less than 5mb so we should just send it :D
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .await
                .context(error::TempSnafu)?;
            self.client
                .put_object()
                .bucket(self.bucket.clone())
                .key(target_path.to_str().unwrap())
                .body(ByteStream::from(buffer))
                .send()
                .await
                .context(error::PutSnafu)?;
        }
        // Now we can delete the temporary file
        tokio::fs::remove_file(&tmp_path)
            .await
            .context(error::TempSnafu)?;
        Ok(layer)
    }

    async fn has_blob(&self, digest: &str) -> StorageResult<bool> {
        // The catalog is the source of truth for what this backend has
        // committed. Mirrors `has` for ids — we deliberately avoid a
        // round-trip HEAD on the blob key because S3 list-after-write is
        // strongly consistent and the catalog is updated under the same
        // best-effort lock as the blob put.
        let catalog = self.load().await?;
        Ok(catalog.has_blob(digest))
    }

    async fn blob_size(&self, digest: &str) -> StorageResult<Option<u64>> {
        let key = self.blob_key().join(digest);
        let key_str = key.to_str().unwrap().to_string();
        match self
            .client
            .head_object()
            .bucket(self.bucket.clone())
            .key(&key_str)
            .send()
            .await
        {
            Ok(resp) => Ok(resp.content_length().map(|n| n.max(0) as u64)),
            Err(e) => {
                // Map a 404 to `None` so callers can distinguish "absent"
                // from "transport/auth failure". `into_service_error`
                // collapses any non-service variants into a panic, so we
                // first peel the service error out manually.
                if let aws_sdk_s3::error::SdkError::ServiceError(ref svc) = e
                    && svc.err().is_not_found()
                {
                    return Ok(None);
                }
                Err(error::CheckSnafu.into_error(e).into())
            }
        }
    }
}
