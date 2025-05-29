mod artifact;
mod backend;
mod catalog;
pub mod error;
mod id;
mod local;

pub use artifact::*;
pub use backend::*;
pub use catalog::*;
use edo_oci::models::Platform;
pub use error::StorageError;
pub use error::StorageResult;
use futures::future::try_join_all;
pub use id::*;
pub use local::*;
use tokio::task::JoinError;

use crate::util::{Reader, Writer};
use indexmap::IndexMap;
use snafu::ResultExt;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

/// Storage represents a handle over multiple caches
/// that represent the storage of artifacts in an edo contexts
#[derive(Clone)]
pub struct Storage {
    // We protect the implementation inside an arced rwlock as we do
    // operate with same storage over multiple tokio routines/threads
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    // Tne local cache is used for storing all things locally that
    // will be needed for a transform. Most operations sync from other caches
    // to this one. It is configurable at the addr //edo-local-cache
    local: Backend,
    // Source caches handle fetching remote artifacts
    // that are used as sources, these are configurable at addr pattern
    // //edo-source-cache/<name>
    source: IndexMap<String, Backend>,
    // A remote build cache allows you to re-use built artifacts from
    // a transform should they exist here.
    // This is configurable at //edo-build-cache
    build: Option<Backend>,
    // An output cache acts as a publish destination for transform results.
    // generally this cache should only ever be pushed to. It is configurable at addr
    // pattern //edo-output-cache
    output: Option<Backend>,
}

// All methods inside inner are actual implementation methods and should return
// cache result, error
impl Inner {
    // Initialize a storage handler, the path here can override where the storage
    // will handle files locally. If provided it willb e turned into an absolute path
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
            component = "storage",
            "registering source cache with name {name}"
        );
        self.source.insert(name.to_string(), cache.clone());
    }

    fn add_source_cache_front(&mut self, name: &str, cache: &Backend) {
        debug!(
            component = "storage",
            "registering source cache at front of priority list with name {name}"
        );
        self.source
            .insert_before(0, name.to_string(), cache.clone());
    }

    fn remove_source_cache(&mut self, name: &str) -> Option<Backend> {
        debug!(
            component = "storage",
            "deregistering source cache with name {name}"
        );
        self.source.shift_remove(name)
    }

    // Set a build cache
    fn set_build_cache(&mut self, cache: &Backend) {
        debug!(component = "storage", "registering a build cache");
        self.build = Some(cache.clone());
    }

    // Set the output cache
    fn set_output_cache(&mut self, cache: &Backend) {
        debug!(component = "storage", "registering an output cache");
        self.output = Some(cache.clone());
    }

    // Open an artifact in the local cache
    async fn safe_open(&self, id: &Id) -> StorageResult<Artifact> {
        debug!(component = "storage", "opening local artifact ({id})");
        self.local.open(id).await
    }

    // Open a layer in the local cache
    async fn safe_read(&self, layer: &Layer) -> StorageResult<Reader> {
        debug!(
            component = "storage",
            "opening local layer ({})",
            layer.digest().digest()
        );
        self.local.read(layer).await
    }

    // Create an artifact in the local cache
    async fn safe_start_layer(&self) -> StorageResult<Writer> {
        debug!(component = "storage", "creating a new local layer");
        self.local.start_layer().await
    }

    // Finish writing a new layer
    async fn safe_finish_layer(
        &self,
        media_type: &MediaType,
        platform: Option<Platform>,
        writer: &Writer,
    ) -> StorageResult<Layer> {
        self.local.finish_layer(media_type, platform, writer).await
    }

    // Save the artifact in the local cache
    async fn safe_save(&self, artifact: &Artifact) -> StorageResult<()> {
        debug!(
            component = "storage",
            "saving artifact ({}) to local cache",
            artifact.config().id()
        );
        self.local.save(artifact).await
    }

    async fn download(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
        // Now we want to in parallel copy all layers
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
                local.finish_layer(layer.media_type(), layer.platform().clone(), &writer).await?;
                Ok(())
            }.instrument(info_span!(target: "storage", "downloading", id = artifact.config().id().to_string(), digest = digest))));
        }
        wait(handles).await?;
        self.local.save(artifact).await?;
        Ok(())
    }

    async fn upload(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
        // Now we want to in parallel copy all layers
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
                backend.finish_layer(layer.media_type(), layer.platform().clone(), &writer).await?;
                Ok(())
            }.instrument(info_span!(target: "storage", "uploading", id = artifact.config().id().to_string(), digest = digest))));
        }
        wait(handles).await?;
        backend.save(artifact).await?;
        Ok(())
    }

    // Fetch a source artifact to the local cache if it doesn't exist,
    // otherwise open it
    async fn fetch_source(&self, id: &Id) -> StorageResult<Option<Artifact>> {
        debug!(
            component = "storage",
            "fetching artifact {id} from source caches"
        );
        if self.local.has(id).await? {
            trace!(
                component = "storage",
                "loading from the local cache as {id} exists already"
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
            component = "storage",
            "fetching artifact {id} from build cache"
        );
        // Check if we already have this artifact locally
        if self.local.has(id).await? {
            trace!(
                component = "storage",
                "loading from the local cache as {id} exists already"
            );
            // No need to sync as its already in the local cache
            return Ok(Some(self.local.open(id).await?));
        }

        // Check if we have registered a build cache and it has this artifact
        if let Some(build) = self.build.as_ref() {
            if build.has(id).await? {
                let artifact = build.open(id).await?;
                if sync {
                    self.download(&artifact, build).await?;
                }
                // Otherwise open it direct from the build cache
                return Ok(Some(artifact));
            }
        }
        // None found
        Ok(None)
    }

    // upload a build artifact if it exists
    async fn upload_build(&self, id: &Id) -> StorageResult<()> {
        // This only occurs if a build cache is registered
        if let Some(build) = self.build.as_ref() {
            debug!(component = "storage", "build cache detected uploading {id}");
            let artifact = self.local.open(id).await?;
            self.upload(&artifact, build).await?;
        }
        Ok(())
    }

    // upload an output artifact if registered to
    #[allow(dead_code)]
    async fn upload_output(&self, id: &Id) -> StorageResult<()> {
        // This only occurs if an output cache is registered
        if let Some(output) = self.output.as_ref() {
            debug!(component = "output cache detected, uploading {id}");
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
}

// The storage methods should always return the parent crate result
impl Storage {
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

    /// Open an artifact stored in the local cache
    /// **safe operation** This operation is safe to call in a networkless environment or in the
    /// build stages as it will make no network calls
    pub async fn safe_open(&self, id: &Id) -> StorageResult<Artifact> {
        self.inner.read().await.safe_open(id).await
    }

    /// Open a layer stored in the local cache
    /// **safe operation** This operation is safe to call in a networkless environment or in the
    /// build stages as it will make no network calls
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
        media_type: &MediaType,
        platform: Option<Platform>,
        writer: &Writer,
    ) -> StorageResult<Layer> {
        self.inner
            .write()
            .await
            .safe_finish_layer(media_type, platform, writer)
            .await
    }

    /// Finish creation of a new local artifact
    pub async fn safe_save(&self, artifact: &Artifact) -> StorageResult<()> {
        self.inner.read().await.safe_save(artifact).await
    }

    /// Fetch a source to local cache and open it for any uses
    /// needed.
    /// **unsafe operation** This operation is unsafe because it could reach out to a networked back source
    /// cache.
    pub async fn fetch_source(&self, id: &Id) -> StorageResult<Option<Artifact>> {
        self.inner.read().await.fetch_source(id).await
    }

    /// Find a source in the source caches
    /// **unsafe operation** This operation is unsafe because it could reach out to a networked back source
    /// cache.
    pub async fn find_source(&self, id: &Id) -> StorageResult<Option<(Artifact, Backend)>> {
        self.inner.read().await.find_source(id).await
    }

    /// Check for a build artifact
    /// **unsafe operation** This operation is unsafe because it could reach out to a remotely backed
    /// build cache.
    pub async fn find_build(&self, id: &Id, sync: bool) -> StorageResult<Option<Artifact>> {
        self.inner.read().await.find_build(id, sync).await
    }

    /// Upload a build artifact if we have a build cache
    pub async fn upload_build(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.upload_build(id).await
    }

    /// Prune the local cache of rerun artifacts
    pub async fn prune_local(&self, id: &Id) -> StorageResult<()> {
        self.inner.read().await.prune_local(id).await
    }

    pub async fn prune_local_all(&self) -> StorageResult<()> {
        self.inner.read().await.prune_local_all().await
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
