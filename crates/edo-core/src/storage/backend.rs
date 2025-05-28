use std::collections::BTreeSet;

use edo_oci::models::Platform;

use crate::def_trait;
use crate::util::{Reader, Writer};

use super::StorageResult;
use super::artifact::MediaType;
use super::{
    artifact::{Artifact, Layer},
    id::Id,
};

def_trait! {
    "This trait represents the interface all storage backend implementations must follow." =>
    "A handle to a given implementation of a storage backend" =>
    Backend: BackendImpl {
        "List all the ids stored in this backend" =>
        list() -> StorageResult<BTreeSet<Id>>;
        "Check if the backend has an artifact by this name" =>
        has(id: &Id) -> StorageResult<bool>;
        "Open an artifact's manifest into memory" =>
        open(id: &Id) -> StorageResult<Artifact>;
        "Save an artifact's manifest" =>
        save(artifact: &Artifact) -> StorageResult<()>;
        "Delete this artifact and all its layers from the backend" =>
        del(id: &Id) -> StorageResult<()>;
        "Copy an artifact to a new id" =>
        copy(from: &Id, to: &Id) -> StorageResult<()>;
        "Prune any other artifact with a different digest from the backend" =>
        prune(id: &Id) -> StorageResult<()>;
        "Prune any duplicate artifacts from the backend" =>
        prune_all() -> StorageResult<()>;
        "Open a reader to a layer" =>
        read(layer: &Layer) -> StorageResult<Reader>;
        "Creates a new layer writer" =>
        start_layer() -> StorageResult<Writer>;
        "Saves and adds a layer to an artifact" =>
        finish_layer(media_type: &MediaType, platform: Option<Platform>, writer: &Writer) -> StorageResult<Layer>
    }
}
