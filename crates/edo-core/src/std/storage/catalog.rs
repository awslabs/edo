use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::storage::{Artifact, Id, Layer};

#[derive(Deserialize, Serialize, Default)]
pub struct Catalog {
    catalog: BTreeMap<String, BTreeSet<Id>>,
    manifests: BTreeMap<Id, Artifact>,
    blob_counts: BTreeMap<String, i64>,
}

impl Catalog {
    pub fn list_all(&self) -> BTreeSet<Id> {
        self.catalog.get("*").cloned().unwrap_or_default()
    }

    pub fn has(&self, id: &Id) -> bool {
        self.manifests.contains_key(id)
    }

    pub fn get(&self, id: &Id) -> Option<&Artifact> {
        self.manifests.get(id)
    }

    pub fn matching(&self, id: &Id) -> BTreeSet<Id> {
        self.catalog.get(&id.prefix()).cloned().unwrap_or_default()
    }

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

    pub fn count(&self, layer: &Layer) -> i64 {
        let digest = layer.digest().digest();
        self.blob_counts.get(&digest).cloned().unwrap_or(0)
    }

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
