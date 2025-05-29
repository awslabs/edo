use async_trait::async_trait;
use edo_core::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo_core::environment::Environment;
use edo_core::source::{SourceImpl, SourceResult};
use edo_core::storage::{
    Artifact, ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, MediaType, Storage,
};
use merkle_hash::MerkleTree;
use snafu::{OptionExt, ResultExt};
use std::path::{Path, PathBuf, absolute};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_tar::Builder;

pub struct LocalSource {
    path: PathBuf,
    out: PathBuf,
    is_archive: bool,
}

#[async_trait]
impl FromNode for LocalSource {
    type Error = error::Error;

    async fn from_node(_: &Addr, node: &Node, _: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["path", "out", "is_archive"])?;
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
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "out",
                type_: "string",
            })?;
        let is_archive = node
            .get("is_archive")
            .unwrap()
            .as_bool()
            .context(error::FieldSnafu {
                field: "is_archive",
                type_: "bool",
            })?;
        Ok(Self {
            path: PathBuf::from(path),
            out: PathBuf::from(out),
            is_archive,
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
        // Local files will never be precached usually
        let digest = base16::encode_lower(hash.as_slice());

        let id = IdBuilder::default()
            .name(
                self.path
                    .file_name()
                    .unwrap_or(self.path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
            )
            .version(None)
            .digest(digest)
            .build()
            .context(error::IdSnafu)?;
        trace!(component = "source", type = "local", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, _log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;

        // First create the manifest
        let mut artifact = ArtifactBuilder::default()
            .config(ConfigBuilder::default().id(id).build().unwrap())
            .build()
            .unwrap();
        // Start our layer
        let mut writer = storage.safe_start_layer().await?;
        // If the path is a file we do that
        let media_type = if self.path.is_file() {
            trace!(component = "source", type = "local", "reading file at {}", self.path.display());
            let mut reader = File::open(&self.path).await.context(error::ReadFileSnafu)?;
            tokio::io::copy(&mut reader, &mut writer)
                .await
                .context(error::ReadFileSnafu)?;
            MediaType::File(Compression::None)
        } else {
            // We want to archive it if its a directory
            trace!(component = "source", type = "local", "archiving directory at {}", self.path.display());
            let mut archive = Builder::new(writer.clone());
            archive
                .append_dir_all(".", &self.path)
                .await
                .context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
            MediaType::Tar(Compression::None)
        };
        writer.flush().await.context(error::ReadFileSnafu)?;
        // Save the layer
        artifact.layers_mut().push(
            storage
                .safe_finish_layer(&media_type, None, &writer)
                .await?,
        );
        // Save the artifact
        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }

    async fn stage(
        &self,
        _log: &Log,
        storage: &Storage,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()> {
        // Staging is rather simple as we just want to move the remote file to the expected location
        let out = path.join(self.out.clone());
        let id = self.get_unique_id().await?;
        // Get the artifact
        let artifact = storage.safe_open(&id).await?;
        let layer = artifact.layers().first().unwrap();
        let reader = storage.safe_read(layer).await?;
        if self.is_archive || matches!(layer.media_type(), MediaType::Tar(..)) {
            trace!(component = "source", type = "local", "staging contents of archive into {}", out.display());
            env.unpack(&out, reader).await?;
        } else {
            trace!(component = "source", type = "local", "staging file to {}", out.display());
            env.write(&out, reader).await?;
        }
        Ok(())
    }
}

pub mod error {
    use edo_core::{plugin::error::PluginError, source::SourceError, storage::IdBuilderError};
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
        #[snafu(display("failed to create id: {source}"))]
        Id { source: IdBuilderError },
        #[snafu(display("failed to calculate merkle hash of directory: {source}"))]
        Merkle {
            source: merkle_hash::error::IndexingError,
        },
        #[snafu(transparent)]
        Project {
            #[snafu(source(from(edo_core::context::ContextError, Box::new)))]
            source: Box<edo_core::context::ContextError>,
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

    impl From<Error> for PluginError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
