use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::storage::{Artifact, Id, Layer};

/// In-memory index of stored artifacts and their reference-counted blobs.
///
/// Tracks manifests by [`Id`], groups them by prefix for pruning, and
/// maintains per-digest reference counts so blobs can be safely deleted
/// when no manifest references them.
///
/// On-disk JSON layout (see `catalog.json` in the `LocalBackend`'s storage
/// root and the analogous object in the `S3Backend`):
///
/// ```text
/// {
///   "prefix_index": { "*": [...ids...], "<prefix>": [...ids...] },
///   "manifests":    { "<id>": { ... artifact ... } },
///   "blob_counts":  { "<digest>": <refcount> }
/// }
/// ```
#[derive(Deserialize, Serialize, Default, Debug)]
pub struct Catalog {
    /// Maps a prefix (`"*"` for the unrestricted view, or [`Id::prefix()`]
    /// for per-prefix listings) to the set of IDs that fall under it.
    /// Used by [`Self::list_all`] and [`Self::matching`].
    prefix_index: BTreeMap<String, BTreeSet<Id>>,
    manifests: BTreeMap<Id, Artifact>,
    blob_counts: BTreeMap<String, i64>,
}

impl Catalog {
    /// List all artifact IDs stored in the catalog.
    pub fn list_all(&self) -> BTreeSet<Id> {
        self.prefix_index.get("*").cloned().unwrap_or_default()
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
        self.prefix_index
            .get(&id.prefix())
            .cloned()
            .unwrap_or_default()
    }

    /// Insert an artifact into the catalog, updating prefix indexes and blob counts.
    pub fn add(&mut self, artifact: &Artifact) {
        let id = artifact.config().id();
        self.prefix_index
            .entry("*".into())
            .or_default()
            .insert(id.clone());
        self.prefix_index
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

    /// Reports whether any saved manifest references a blob with this bare
    /// hex digest. This is purely an in-memory hint — callers that need
    /// a stronger guarantee (e.g. that the blob actually exists on disk)
    /// must perform their own filesystem-level check.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_counts.contains_key(digest)
    }

    /// Remove an artifact from the catalog, decrementing blob reference counts.
    pub fn del(&mut self, id: &Id) {
        self.prefix_index.entry("*".into()).or_default().remove(id);
        if let Some(list) = self.prefix_index.get_mut(&id.prefix()) {
            list.remove(id);
            if list.is_empty() {
                self.prefix_index.remove(&id.prefix());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Artifact, Compression, Config, Layer, MediaType};

    fn artifact(name: &str, digest: &str, layer_digests: &[&str]) -> Artifact {
        let id = Id::builder()
            .name(name.to_string())
            .digest(digest.to_string())
            .build();
        let mut a = Artifact::builder()
            .media_type(MediaType::Manifest)
            .config(Config::builder().id(id).build())
            .build();
        for d in layer_digests {
            a.layers_mut().push(
                Layer::builder()
                    .media_type(MediaType::File(Compression::None))
                    .digest((*d).to_string())
                    .size(0usize)
                    .build(),
            );
        }
        a
    }

    #[test]
    fn add_then_del_returns_to_empty_state() {
        let mut c = Catalog::default();
        let a = artifact("foo", "deadbeef", &["aaaa"]);
        c.add(&a);
        assert!(c.has(a.config().id()));
        assert!(c.has_blob("aaaa"));
        c.del(a.config().id());
        assert!(!c.has(a.config().id()));
        assert!(!c.has_blob("aaaa"), "blob count should be removed");
        assert!(
            !c.list_all().iter().any(|x| x == a.config().id()),
            "id should be gone from list_all"
        );
    }

    #[test]
    fn shared_blob_refcount_tracked() {
        let mut c = Catalog::default();
        let a = artifact("foo", "111", &["shared"]);
        let b = artifact("bar", "222", &["shared"]);
        c.add(&a);
        c.add(&b);
        let probe = Layer::builder()
            .media_type(MediaType::File(Compression::None))
            .digest("shared".to_string())
            .size(0usize)
            .build();
        assert_eq!(c.count(&probe), 2);
        c.del(a.config().id());
        assert_eq!(c.count(&probe), 1);
        assert!(c.has_blob("shared"));
        c.del(b.config().id());
        assert_eq!(c.count(&probe), 0);
        assert!(!c.has_blob("shared"));
    }

    #[test]
    fn prefix_index_emptied_when_last_id_removed() {
        let mut c = Catalog::default();
        let a = artifact("foo", "111", &[]);
        c.add(&a);
        assert!(!c.matching(a.config().id()).is_empty());
        c.del(a.config().id());
        assert!(
            c.matching(a.config().id()).is_empty(),
            "prefix bucket should be empty after del of last id"
        );
        assert!(c.list_all().is_empty(), "list_all should also be empty");
    }

    #[test]
    fn round_trips_through_serde() {
        // Deserializing an `Id` parses it via `FromStr`, which requires a
        // 64-hex SHA256 digest. Use real-shape digests here so the round
        // trip exercises the parser, not just the builder.
        const D1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const D2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
        let mut c = Catalog::default();
        c.add(&artifact("foo", D1, &["aaa"]));
        c.add(&artifact("bar", D2, &["bbb", "ccc"]));
        let bytes = serde_json::to_vec(&c).expect("serialize");
        let back: Catalog = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back.list_all().len(), 2);
        assert!(back.has_blob("aaa"));
        assert!(back.has_blob("bbb"));
        assert!(back.has_blob("ccc"));
    }
}
