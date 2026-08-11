use std::collections::BTreeSet;

use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
#[allow(unused_imports)]
use mockall::automock;

use crate::storage::LayerOptions;
use crate::util::{Reader, Writer};

use super::StorageResult;
use super::{
    artifact::{Artifact, Layer},
    id::Id,
};

/// The low-level interface for storing and retrieving artifacts in a location.
///
/// Implementations handle the physical persistence of manifests and layer
/// blobs (e.g. local filesystem, S3, OCI registry). [`Storage`](super::Storage)
/// composes multiple backends into a layered cache hierarchy.
#[arc_handle]
#[async_trait]
pub trait Backend {
    /// List all the ids stored in this backend
    async fn list(&self) -> StorageResult<BTreeSet<Id>>;
    /// Check if the backend has an artifact by this name
    async fn has(&self, id: &Id) -> StorageResult<bool>;
    /// Open an artifact's manifest into memory
    async fn open(&self, id: &Id) -> StorageResult<Artifact>;
    /// Save an artifact's manifest
    async fn save(&self, artifact: &Artifact) -> StorageResult<()>;
    /// Delete this artifact and all its layers from the backend
    async fn del(&self, id: &Id) -> StorageResult<()>;
    /// Copy an artifact to a new id
    async fn copy(&self, from: &Id, to: &Id) -> StorageResult<()>;
    /// Prune any other artifact with a different digest from the backend
    async fn prune(&self, id: &Id) -> StorageResult<()>;
    /// Prune any duplicate artifacts from the backend
    async fn prune_all(&self) -> StorageResult<()>;
    /// Open a reader to a layer
    async fn read(&self, layer: &Layer) -> StorageResult<Reader>;
    /// Creates a new layer writer for an artifact
    async fn start_layer(&self) -> StorageResult<Writer>;
    /// Saves and adds a layer to an artifact
    async fn finish_layer(&self, writer: &Writer, options: &LayerOptions) -> StorageResult<Layer>;
    /// Reports whether a blob with the given bare hex digest is already
    /// stored by this backend, without performing IO outside the backend's
    /// own state.
    async fn has_blob(&self, _digest: &str) -> StorageResult<bool>;
    /// Return the size in bytes of a stored blob, if present.
    ///
    /// Used by content-addressed sources to populate accurate `size`
    /// fields when reusing an existing blob (the manifest is the
    /// source-of-truth for the data model and lying about size is a
    /// footgun for any future consumer that range-reads by size).
    async fn blob_size(&self, _digest: &str) -> StorageResult<Option<u64>>;
}
