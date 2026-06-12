use async_trait::async_trait;
use edo::context::{Context, Element, FromElement, Log};
use edo::record;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};
use edo::util::cmd_noinput;
use snafu::ResultExt;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tracing::Instrument;

/// A source that clones a Git repository at a specific reference.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    url: String,
    #[serde(rename = "ref")]
    reference: String,
    out: Option<PathBuf>,
}

#[async_trait]
impl FromElement for GitSource {
    type Error = error::Error;

    async fn new(element: &Element, _: &Context) -> Result<Self, error::Error> {
        element.get().map_err(|e| e.into())
    }
}

#[async_trait]
impl SourceImpl for GitSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // Fold `out` into the digest so changing the staging path
        // invalidates the cached manifest. The name is kept stable
        // (no longer embeds `out`) so the human-facing id stays clean.
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.reference.as_bytes());
        hasher.update(
            self.out
                .as_ref()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        let digest = base16::encode_lower(hasher.finalize().as_bytes());
        let id = Id::builder()
            .name(format!("{}@{}", self.url, self.reference))
            .digest(digest)
            .build();
        trace!(subsystem = "source", component = "git", id = %id, "calculated id");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        info!(
            subsystem = "source",
            component = "git",
            op = "fetch",
            id = %id,
            url = %self.url,
            reference = %self.reference,
            "cloning {}@{}",
            self.url,
            self.reference
        );
        record!(log, "clone", "git clone -b {} {}", self.reference, self.url);
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
            let mut artifact = Artifact::builder()
                .media_type(MediaType::Manifest)
                .config(
                    Config::builder()
                        .metadata(serde_json::json!({
                            "repository": self.url,
                            "reference": self.reference
                        }))
                        .id(id.clone())
                        .build(),
                )
                .build();

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
            let layer = storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(Compression::None))
                        .build(),
                )
                .await?;
            // Record `out` as the artifact-level staging hint keyed by
            // the layer's digest. See `Config::path_hints`.
            if let Some(hint) = self.out.clone() {
                artifact
                    .config_mut()
                    .path_hints_mut()
                    .insert(layer.digest().digest(), hint);
            }
            artifact.layers_mut().push(layer);
            // Now save the artifact itself
            storage.safe_save(&artifact).await?;
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "source-fetch",
            subsystem = "source",
            component = "git",
            id = %id_s,
            url = %self.url,
            reference = %self.reference
        ))
        .await
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
        #[snafu(display("failed to archive git repository: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("invalid git source definition for {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("failed to invoke git cli: {source}"))]
        Git { source: std::io::Error },
        #[snafu(transparent)]
        Project {
            #[snafu(source(from(edo::context::ContextError, Box::new)))]
            source: Box<edo::context::error::ContextError>,
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

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
