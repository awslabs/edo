//! Storage subsystem.
//!
//! Provides a layered, content-addressable artifact store modelled after OCI
//! image layouts. [`Storage`] orchestrates multiple [`Backend`] caches (local,
//! source, build, output) while [`Artifact`], [`Layer`], and [`Id`] describe
//! the data model. The default [`LocalBackend`] persists blobs on the
//! filesystem using SHA256 digests.
//!
//! All fallible operations return [`StorageResult`], with failures modelled by
//! [`StorageError`].

mod artifact;
mod backend;
mod catalog;
pub mod error;
mod id;
mod local;

pub use artifact::*;
pub use backend::*;
pub use catalog::*;
pub use error::StorageError;
pub use error::StorageResult;
use futures::future::try_join_all;
pub use id::*;
pub use local::*;
use tokio::task::JoinError;

use crate::util::{Reader, Writer};
use indexmap::IndexMap;
use snafu::ResultExt;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

/// Handle over multiple layered artifact caches.
///
/// Orchestrates a local cache, zero-or-more source caches (priority-ordered),
/// an optional build cache, and an optional output cache. All layer data is
/// content-addressed by SHA256 digest.
#[derive(Clone)]
pub struct Storage {
    // Serialize cache configuration changes across tasks.
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    // Local artifact and blob cache: //edo-local-cache.
    local: Backend,
    // Ordered source caches: //edo-source-cache/<name>.
    source: IndexMap<String, Backend>,
    // Optional build cache: //edo-build-cache.
    build: Option<Backend>,
    // Optional publication destination: //edo-output-cache.
    output: Option<Backend>,
}

impl Inner {
    // Initialize the local cache.
    async fn init(backend: Backend) -> StorageResult<Self> {
        Ok(Self {
            local: backend,
            source: IndexMap::new(),
            build: None,
            output: None,
        })
    }

    // Add a source cache
    fn add_source_cache(&mut self, name: &str, cache: &Backend) {
        debug!(
            subsystem = "storage",
            op = "register",
            name = %name,
            "registering source cache"
        );
        self.source.insert(name.to_string(), cache.clone());
    }

    fn add_source_cache_front(&mut self, name: &str, cache: &Backend) {
        debug!(
            subsystem = "storage",
            op = "register",
            name = %name,
            "registering source cache at front of priority list"
        );
        self.source
            .insert_before(0, name.to_string(), cache.clone());
    }

    fn remove_source_cache(&mut self, name: &str) -> Option<Backend> {
        debug!(
            subsystem = "storage",
            op = "deregister",
            name = %name,
            "deregistering source cache"
        );
        self.source.shift_remove(name)
    }

    // Set a build cache
    fn set_build_cache(&mut self, cache: &Backend) {
        debug!(
            subsystem = "storage",
            op = "register",
            "registering a build cache"
        );
        self.build = Some(cache.clone());
    }

    // Set the output cache
    fn set_output_cache(&mut self, cache: &Backend) {
        debug!(
            subsystem = "storage",
            op = "register",
            "registering an output cache"
        );
        self.output = Some(cache.clone());
    }

    // List the ids in the local cache
    async fn safe_list(&self) -> StorageResult<BTreeSet<Id>> {
        self.local.list().await
    }

    // Open an artifact in the local cache
    async fn safe_open(&self, id: &Id) -> StorageResult<Artifact> {
        debug!(subsystem = "storage", id = %id, "opening local artifact");
        self.local.open(id).await
    }

    // Open a layer in the local cache
    async fn safe_read(&self, layer: &Layer) -> StorageResult<Reader> {
        debug!(
            subsystem = "storage",
            digest = %layer.digest().digest(),
            "opening local layer"
        );
        self.local.read(layer).await
    }

    // Create an artifact in the local cache
    async fn safe_start_layer(&self) -> StorageResult<Writer> {
        debug!(subsystem = "storage", "creating a new local layer");
        self.local.start_layer().await
    }

    // Finish writing a new layer
    async fn safe_finish_layer(
        &self,
        writer: &Writer,
        options: &LayerOptions,
    ) -> StorageResult<Layer> {
        self.local.finish_layer(writer, options).await
    }

    // Save the artifact in the local cache
    async fn safe_save(&self, artifact: &Artifact) -> StorageResult<()> {
        debug!(
            subsystem = "storage",
            id = %artifact.config().id(),
            "saving artifact to local cache"
        );
        self.local.save(artifact).await
    }

    async fn download(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
        // Copy layers concurrently.
        let mut handles = Vec::new();
        for layer in artifact.layers() {
            let backend = backend.clone();
            let local = self.local.clone();
            let layer = layer.clone();
            let digest = layer.digest().digest();
            handles.push(tokio::spawn(async move {
                let layer = layer.clone();
                let mut reader = backend.read(&layer).await?;
                let mut writer = local.start_layer().await?;
                tokio::io::copy(&mut reader, &mut writer).await.context(error::IoSnafu)?;
                local.finish_layer(
                    &writer, &LayerOptions::builder()
                        .media_type(layer.media_type().clone())
                        .maybe_platform(layer.platform().clone())
                        .build()).await?;
                Ok(())
            }.instrument(info_span!("cache-download", subsystem = "storage", op = "download", id = %artifact.config().id(), digest = %digest))));
        }
        wait(handles).await?;
        self.local.save(artifact).await?;
        Ok(())
    }

    async fn upload(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
        // Copy layers concurrently.
        let mut handles = Vec::new();
        for layer in artifact.layers() {
            let backend = backend.clone();
            let local = self.local.clone();
            let layer = layer.clone();
            let digest = layer.digest().digest();
            handles.push(tokio::spawn(async move {
                let layer = layer.clone();
                let mut reader = local.read(&layer).await?;
                let mut writer = backend.start_layer().await?;
                tokio::io::copy(&mut reader, &mut writer).await.context(error::IoSnafu)?;
                backend.finish_layer(&writer, &LayerOptions::builder()
                    .media_type(layer.media_type().clone())
                    .maybe_platform(layer.platform().clone())
                    .build()
                ).await?;
                Ok(())
            }.instrument(info_span!("cache-upload", subsystem = "storage", op = "upload", id = %artifact.config().id(), digest = %digest))));
        }
        wait(handles).await?;
        backend.save(artifact).await?;
        Ok(())
    }

    // Fetch a source artifact to the local cache if it doesn't exist,
    // otherwise open it
    async fn fetch_source(&self, id: &Id) -> StorageResult<Option<Artifact>> {
        debug!(
            subsystem = "storage",
            op = "fetch",
            id = %id,
            "fetching artifact from source caches"
        );
        if self.local.has(id).await? {
            trace!(
                subsystem = "storage",
                op = "cache-hit",
                id = %id,
                "loading from the local cache as artifact exists already"
            );
            return Ok(Some(self.local.open(id).await?));
        }
        if let Some((artifact, backend)) = self.find_source(id).await? {
            self.download(&artifact, &backend).await?;
            Ok(Some(artifact))
        } else {
            Ok(None)
        }
    }

    // Find a source artifact in the source caches by the priority of the order of the
    // source caches
    async fn find_source(&self, id: &Id) -> StorageResult<Option<(Artifact, Backend)>> {
        for (_, cache) in self.source.iter() {
            if cache.has(id).await? {
                return Ok(Some((cache.open(id).await?, cache.clone())));
            }
        }
        Ok(None)
    }

    // Check for a build artifact, if found we will synchronize it to the local cache if
    // asked to
    async fn find_build(&self, id: &Id, sync: bool) -> StorageResult<Option<Artifact>> {
        debug!(
            subsystem = "storage",
            op = "fetch",
            target = "build-cache",
            id = %id,
            "fetching artifact from build cache"
        );
        // Check if we already have this artifact locally
        if self.local.has(id).await? {
            trace!(
                subsystem = "storage",
                op = "cache-hit",
                id = %id,
                "loading from the local cache as artifact exists already"
            );
            return Ok(Some(self.local.open(id).await?));
        }

        // Check if we have registered a build cache and it has this artifact
        if let Some(build) = self.build.as_ref()
            && build.has(id).await?
        {
            let artifact = build.open(id).await?;
            if sync {
                self.download(&artifact, build).await?;
            }
            return Ok(Some(artifact));
        }
        Ok(None)
    }

    // upload a build artifact if it exists
    async fn upload_build(&self, id: &Id) -> StorageResult<()> {
        if let Some(build) = self.build.as_ref() {
            crate::ui_info!(
                subsystem = "storage",
                component = "build-cache",
                op = "upload",
                id = id;
                "build cache detected, uploading"
            );
            let artifact = self.local.open(id).await?;
            self.upload(&artifact, build).await?;
        }
        Ok(())
    }

    // upload an output artifact if registered to
    #[allow(dead_code)]
    async fn upload_output(&self, id: &Id) -> StorageResult<()> {
        if let Some(output) = self.output.as_ref() {
            crate::ui_info!(
                subsystem = "storage",
                component = "output-cache",
                op = "upload",
                id = id;
                "output cache detected, uploading"
            );
            let artifact = self.local.open(id).await?;
            self.upload(&artifact, output).await?;
        }
        Ok(())
    }

    pub async fn prune_local(&self, id: &Id) -> StorageResult<()> {
        self.local.prune(id).await
    }

    pub async fn prune_local_all(&self) -> StorageResult<()> {
        self.local.prune_all().await
    }

    pub async fn drop_local(&self, id: &Id) -> StorageResult<()> {
        self.local.del(id).await
    }

    pub async fn drop_local_matching(&self, id: &Id) -> StorageResult<()> {
        // Release the listing lock before deleting entries.
        let matching = self.local.list().await?;
        let prefix = id.prefix();
        for entry in matching {
            if entry.prefix() == prefix {
                self.local.del(&entry).await?;
            }
        }
        Ok(())
    }
}

impl Storage {
    /// Initialize storage with the given backend as the local cache.
    pub async fn init(backend: &Backend) -> StorageResult<Self> {
        Ok(Self {
            inner: Arc::new(RwLock::new(Inner::init(backend.clone()).await?)),
        })
    }

    /// Add a new source cache to the end of the priority list
    pub async fn add_source_cache(&self, name: &str, cache: &Backend) {
        self.inner.write().await.add_source_cache(name, cache);
    }

    /// Add a new source cache to the front of the priority list
    pub async fn add_source_cache_front(&self, name: &str, cache: &Backend) {
        self.inner.write().await.add_source_cache_front(name, cache);
    }

    /// Remove a source cache
    pub async fn remove_source_cache(&self, name: &str) -> Option<Backend> {
        self.inner.write().await.remove_source_cache(name)
    }

    /// Set the build cache
    pub async fn set_build(&self, cache: &Backend) {
        self.inner.write().await.set_build_cache(cache);
    }

    /// Set the output cache
    pub async fn set_output(&self, cache: &Backend) {
        self.inner.write().await.set_output_cache(cache);
    }

    /// Checks whether `id` is cached locally without network access.
    pub async fn has_local(&self, id: &Id) -> StorageResult<bool> {
        self.inner.read().await.local.has(id).await
    }

    /// Checks for a local blob by bare hex digest without network access.
    pub async fn has_local_blob(&self, digest: &str) -> StorageResult<bool> {
        self.inner.read().await.local.has_blob(digest).await
    }

    /// Returns a local blob's size, if present, without network access.
    pub async fn local_blob_size(&self, digest: &str) -> StorageResult<Option<u64>> {
        self.inner.read().await.local.blob_size(digest).await
    }

    /// Lists local artifacts without network access.
    pub async fn safe_list(&self) -> StorageResult<BTreeSet<Id>> {
        self.inner.read().await.safe_list().await
    }

    /// Opens a local artifact without network access.
    pub async fn safe_open(&self, id: &Id) -> StorageResult<Artifact> {
        self.inner.read().await.safe_open(id).await
    }

    /// Opens a local layer without network access.
    pub async fn safe_read(&self, layer: &Layer) -> StorageResult<Reader> {
        self.inner.read().await.safe_read(layer).await
    }

    /// All new artifacts should be created first in the local cache with safe_create
    pub async fn safe_start_layer(&self) -> StorageResult<Writer> {
        self.inner.read().await.safe_start_layer().await
    }

    /// Finish writing of a local layer
    pub async fn safe_finish_layer(
        &self,
        writer: &Writer,
        options: &LayerOptions,
    ) -> StorageResult<Layer> {
        self.inner
            .write()
            .await
            .safe_finish_layer(writer, options)
            .await
    }

    /// Finish creation of a new local artifact
    pub async fn safe_save(&self, artifact: &Artifact) -> StorageResult<()> {
        self.inner.read().await.safe_save(artifact).await
    }

    /// Fetches and opens a source locally; may access networked caches.
    pub async fn fetch_source(&self, id: &Id) -> StorageResult<Option<Artifact>> {
        self.inner.read().await.fetch_source(id).await
    }

    /// Finds a source in priority order; may access networked caches.
    pub async fn find_source(&self, id: &Id) -> StorageResult<Option<(Artifact, Backend)>> {
        self.inner.read().await.find_source(id).await
    }

    /// Finds a build artifact; may access the networked build cache.
    pub async fn find_build(&self, id: &Id, sync: bool) -> StorageResult<Option<Artifact>> {
        self.inner.read().await.find_build(id, sync).await
    }

    /// Upload a build artifact if we have a build cache
    pub async fn upload_build(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.upload_build(id).await
    }

    /// Prune the local cache of all artifacts sharing a prefix with `id` except `id` itself.
    pub async fn prune_local(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.prune_local(id).await
    }

    /// Remove all artifacts and blobs from the local cache.
    pub async fn prune_local_all(&self) -> StorageResult<()> {
        self.inner.read().await.prune_local_all().await
    }

    /// Removes `id` from the local cache, retaining shared blobs; missing ids are a no-op.
    pub async fn drop_local(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.drop_local(id).await
    }

    /// Removes all local artifacts sharing `id`'s prefix, retaining shared blobs.
    pub async fn drop_local_matching(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.drop_local_matching(id).await
    }
}

async fn wait<I, R>(handles: I) -> StorageResult<Vec<R>>
where
    R: Clone,
    I: IntoIterator,
    I::Item: Future<Output = std::result::Result<StorageResult<R>, JoinError>>,
{
    let result = try_join_all(handles).await;
    let mut success = Vec::new();
    let mut failures = Vec::new();
    for entry in result.context(error::JoinSnafu)? {
        match entry {
            Ok(result) => success.push(result),
            Err(e) => failures.push(e),
        }
    }
    if !failures.is_empty() {
        error::ChildSnafu { children: failures }.fail()
    } else {
        Ok(success)
    }
}
