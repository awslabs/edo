use crate::context::{non_configurable, Addr, Context, FromNode, Log, Node};
use crate::environment::Environment;
use crate::source::{SourceImpl, SourceResult};
use crate::storage::{
    Artifact, ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, MediaType, Storage,
};
use crate::util::cmd_noinput;
use async_trait::async_trait;
use snafu::{OptionExt, ResultExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tracing::Instrument;

pub struct GitSource {
    url: String,
    reference: String,
    out: PathBuf,
}

#[async_trait]
impl FromNode for GitSource {
    type Error = error::Error;

    async fn from_node(_addr: &Addr, node: &Node, _: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["url", "ref", "out"])?;
        let url = node
            .get("url")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "url",
                type_: "string",
            })?;
        let reference = node
            .get("ref")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "ref",
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
        Ok(Self {
            url,
            reference,
            out: PathBuf::from(out),
        })
    }
}

non_configurable!(GitSource, error::Error);

#[async_trait]
impl SourceImpl for GitSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        let id = IdBuilder::default()
            .name(format!("{}@{}", self.url, self.reference))
            .version(None)
            .digest(base16::encode_lower(self.reference.as_bytes()))
            .build()
            .context(error::IdSnafu)?;
        trace!(component = "source", type = "git", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        trace!(component = "source", type = "git", "cloning git repository: git clne -b {} {}", self.reference, self.url);
        async move {
            let temp = tempdir().context(error::TempDirectorySnafu)?;
            cmd_noinput(
                ".",
                log,
                "git",
                vec![
                    "clone".into(),
                    "-b".into(),
                    self.reference.clone(),
                    self.url.clone(),
                    temp.path().to_string_lossy().to_string(),
                ],
                &HashMap::new(),
            )
            .context(error::GitSnafu)?;
            // Make our initial artifact manifest
            let mut artifact = ArtifactBuilder::default()
                .media_type(MediaType::Manifest)
                .config(
                    ConfigBuilder::default()
                        .metadata(serde_json::json!({
                            "repository": self.url,
                            "reference": self.reference
                        }))
                        .id(id.clone())
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap();

            // Now we want to open a single layer which we will archive the source
            let mut writer = storage.safe_start_layer().await?;
            let mut archive = tokio_tar::Builder::new(writer.clone());
            archive
                .append_dir_all(".", temp.path())
                .await
                .context(error::ArchiveSnafu)?;
            writer.flush().await.context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
            // Now we can add the the layer to the artifact
            artifact.layers_mut().push(
                storage
                    .safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer)
                    .await?,
            );
            // Now save the artifact itself
            storage.safe_save(&artifact).await?;
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "fetching",
            id = id_s,
            log = log.log_name(),
            component = "source"
        ))
        .await
    }

    async fn stage(
        &self,
        _log: &Log,
        storage: &Storage,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()> {
        let out_path = path.join(self.out.clone());
        trace!(component = "source", type = "git", "staging into {}", out_path.display());
        // We want to open the artifact manifest first
        let id = self.get_unique_id().await?;
        let artifact = storage.safe_open(&id).await?;
        // There should only be 1 layer that is our target
        let reader = storage
            .safe_read(artifact.layers().first().unwrap())
            .await?;
        env.unpack(&out_path, reader).await?;
        Ok(())
    }
}

pub mod error {
    use crate::{plugin::error::PluginError, source::SourceError, storage::IdBuilderError};
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to archive git repository: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("git source definition field '{field}' should be a '{type_}'"))]
        Field { field: String, type_: String },
        #[snafu(display("failed to invoke git cli: {source}"))]
        Git { source: std::io::Error },
        #[snafu(display("failed to create id: {source}"))]
        Id { source: IdBuilderError },
        #[snafu(transparent)]
        Project {
            #[snafu(source(from(crate::context::ContextError, Box::new)))]
            source: Box<crate::context::error::ContextError>,
        },
        #[snafu(display("failed to create temporary directory: {source}"))]
        TempDirectory { source: std::io::Error },
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
