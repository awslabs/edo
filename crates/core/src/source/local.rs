use async_trait::async_trait;
use edo::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo::record;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};
use merkle_hash::MerkleTree;
use snafu::{OptionExt, ResultExt};
use std::path::{PathBuf, absolute};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_tar::Builder;

/// A source backed by a local filesystem path.
pub struct LocalSource {
    path: PathBuf,
    out: Option<PathBuf>,
}

/// Folds an optional `out` value into a hash so two otherwise-identical
/// sources with different `out`s produce different ids. Empty/missing
/// `out` hashes a stable empty marker so old ids stay deterministic.
fn out_bytes(out: Option<&PathBuf>) -> Vec<u8> {
    out.and_then(|p| p.to_str())
        .unwrap_or("")
        .as_bytes()
        .to_vec()
}

#[async_trait]
impl FromNode for LocalSource {
    type Error = error::Error;

    async fn from_node(_: &Addr, node: &Node, _: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["path"])?;
        let path = node
            .get("path")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "path",
                type_: "string",
            })?;
        let out = node
            .get("out")
            .and_then(|x| x.as_string())
            .map(PathBuf::from);
        Ok(Self {
            path: PathBuf::from(path),
            out,
        })
    }
}

non_configurable!(LocalSource, error::Error);

#[async_trait]
impl SourceImpl for LocalSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // The digest should be calculated as a merkle hash of the source files
        let apath = absolute(&self.path).context(error::AbsoluteSnafu)?;
        let merkle = MerkleTree::builder(apath.to_string_lossy().as_ref())
            .build()
            .context(error::MerkleSnafu)?;
        let hash = merkle.root.item.hash;
        // Fold `out` into the manifest digest so `out` changes invalidate
        // the cached manifest. The blob itself is still derived purely
        // from the file content; only the *manifest* id changes.
        let mut hasher = blake3::Hasher::new();
        hasher.update(hash.as_slice());
        hasher.update(&out_bytes(self.out.as_ref()));
        let digest = base16::encode_lower(hasher.finalize().as_bytes());

        let id = Id::builder()
            .name(
                self.path
                    .file_name()
                    .unwrap_or(self.path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
            )
            .digest(digest)
            .build();
        trace!(component = "source", type = "local", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;

        // First create the manifest
        let mut artifact = Artifact::builder()
            .media_type(MediaType::File(Compression::None))
            .config(Config::builder().id(id).build())
            .build();
        // Start our layer
        let mut writer = storage.safe_start_layer().await?;
        // If the path is a file we do that
        let (media_type, path_hint) = if self.path.is_file() {
            trace!(component = "source", type = "local", "reading file at {}", self.path.display());
            let mut reader = File::open(&self.path).await.context(error::ReadFileSnafu)?;
            record!(log, "copy", "storing file from {:?}", self.path);
            tokio::io::copy(&mut reader, &mut writer)
                .await
                .context(error::ReadFileSnafu)?;
            // Detect the media type from the filename so local archives
            // (.tar, .tar.gz, .tgz, .zip, ...) are extracted at stage
            // time rather than copied verbatim.
            let filename = self
                .path
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            let media_type = MediaType::detect(&filename)?;
            let path_hint = self.out.clone().or_else(|| {
                self.path
                    .file_name()
                    .map(|x| x.to_str().unwrap())
                    .map(PathBuf::from)
            });
            (media_type, path_hint)
        } else {
            // We want to archive it if its a directory
            trace!(component = "source", type = "local", "archiving directory at {}", self.path.display());
            record!(
                log,
                "archive",
                "archiving contents of directory at {:?}",
                self.path
            );
            let mut archive = Builder::new(writer.clone());
            archive.mode(tokio_tar::HeaderMode::Complete);
            archive
                .append_dir_all(".", &self.path)
                .await
                .context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
            (MediaType::Tar(Compression::None), self.out.clone())
        };
        writer.flush().await.context(error::ReadFileSnafu)?;
        // Save the layer
        let layer = storage
            .safe_finish_layer(
                &writer,
                &LayerOptions::builder().media_type(media_type).build(),
            )
            .await?;
        // Record the staging hint at the artifact level keyed by the layer
        // digest (matches `Catalog::blob_counts` and `LayerDigest::digest()`).
        if let Some(hint) = path_hint {
            artifact
                .config_mut()
                .path_hints_mut()
                .insert(layer.digest().digest(), hint);
        }
        artifact.layers_mut().push(layer);
        // Save the artifact
        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }
}

pub mod error {
    use edo::{context::error::ContextError, source::SourceError};
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to resolve path into absolute path: {source}"))]
        Absolute { source: std::io::Error },
        #[snafu(display("failed to archive git repository: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("local source definition field '{field}' should be a '{type_}'"))]
        Field { field: String, type_: String },
        #[snafu(display("failed to calculate merkle hash of directory: {source}"))]
        Merkle {
            source: merkle_hash::error::IndexingError,
        },
        #[snafu(transparent)]
        Project {
            #[snafu(source(from(edo::context::ContextError, Box::new)))]
            source: Box<edo::context::ContextError>,
        },
        #[snafu(display("failed to read a file: {source}"))]
        ReadFile { source: std::io::Error },
    }

    impl From<Error> for SourceError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
