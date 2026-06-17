use async_trait::async_trait;
use edo::context::{Context, Element, FromElement, Log};
use edo::record;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};
use merkle_hash::MerkleTree;
use snafu::ResultExt;
use std::path::{PathBuf, absolute};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_tar::Builder;

/// A source backed by a local filesystem path.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct LocalSource {
    path: PathBuf,
    out: Option<PathBuf>,
}

#[async_trait]
impl FromElement for LocalSource {
    type Error = error::Error;

    async fn new(element: &Element, _: &Context) -> Result<Self, error::Error> {
        element.get().map_err(|e| e.into())
    }
}

#[async_trait]
impl SourceImpl for LocalSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // The digest should be calculated as a merkle hash of the source files
        let apath = absolute(&self.path).context(error::AbsoluteSnafu)?;
        let merkle = MerkleTree::builder(apath.to_string_lossy().as_ref())
            .build()
            .context(error::MerkleSnafu)?;
        let hash = merkle.root.item.hash;
        // Local files will never be precached usually
        let digest = base16::encode_lower(hash.as_slice());

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
        artifact.layers_mut().push(
            storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(media_type)
                        .maybe_path_hint(path_hint)
                        .build(),
                )
                .await?,
        );
        // Save the artifact
        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }
}

pub mod error {
    use edo::{
        context::{Addr, error::ContextError},
        source::SourceError,
    };
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to resolve path into absolute path: {source}"))]
        Absolute { source: std::io::Error },
        #[snafu(display("failed to archive git repository: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("invalid local source definition {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
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
