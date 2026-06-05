use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::storage::{Artifact, Id, Layer};

/// In-memory index of stored artifacts and their reference-counted blobs.
///
/// Tracks manifests by [`Id`], groups them by prefix for pruning, and
/// maintains per-digest reference counts so blobs can be safely deleted
/// when no manifest references them.
#[derive(Deserialize, Serialize, Default, Debug)]
pub struct Catalog {
    catalog: BTreeMap<String, BTreeSet<Id>>,
    manifests: BTreeMap<Id, Artifact>,
    blob_counts: BTreeMap<String, i64>,
}

impl Catalog {
    /// List all artifact IDs stored in the catalog.
    pub fn list_all(&self) -> BTreeSet<Id> {
        self.catalog.get("*").cloned().unwrap_or_default()
    }

    /// Returns `true` if the catalog contains a manifest for the given `id`.
    pub fn has(&self, id: &Id) -> bool {
        self.manifests.contains_key(id)
    }

    /// Retrieve a reference to the artifact manifest for `id`, if present.
    pub fn get(&self, id: &Id) -> Option<&Artifact> {
        self.manifests.get(id)
    }

    /// Return all artifact IDs that share the same prefix as `id`.
    pub fn matching(&self, id: &Id) -> BTreeSet<Id> {
        self.catalog.get(&id.prefix()).cloned().unwrap_or_default()
    }

    /// Insert an artifact into the catalog, updating prefix indexes and blob counts.
    pub fn add(&mut self, artifact: &Artifact) {
        let id = artifact.config().id();
        self.catalog
            .entry("*".into())
            .or_default()
            .insert(id.clone());
        self.catalog
            .entry(id.prefix())
            .or_default()
            .insert(id.clone());
        self.manifests.insert(id.clone(), artifact.clone());
        for layer in artifact.layers() {
            let digest = layer.digest().digest();
            *self.blob_counts.entry(digest).or_default() += 1;
        }
    }

    /// Return the reference count for the blob backing `layer`.
    pub fn count(&self, layer: &Layer) -> i64 {
        let digest = layer.digest().digest();
        self.blob_counts.get(&digest).cloned().unwrap_or(0)
    }

    /// Remove an artifact from the catalog, decrementing blob reference counts.
    pub fn del(&mut self, id: &Id) {
        self.catalog.entry("*".into()).or_default().remove(id);
        if let Some(list) = self.catalog.get_mut(&id.prefix()) {
            list.remove(id);
            if list.is_empty() {
                self.catalog.remove(&id.prefix());
            }
        }
        if let Some(artifact) = self.manifests.remove(id) {
            for layer in artifact.layers() {
                let digest = layer.digest().digest();
                if let Some(blob_count) = self.blob_counts.get_mut(&digest) {
                    *blob_count -= 1;
                    if *blob_count <= 0 {
                        self.blob_counts.remove(&digest);
                    }
                }
            }
        }
    }
}
