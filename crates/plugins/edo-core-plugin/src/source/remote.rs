use async_trait::async_trait;
use futures::TryStreamExt;
use serde_json::json;
use snafu::{OptionExt, ResultExt, ensure};
use std::path::Path;
use std::path::PathBuf;
use tokio_util::io::StreamReader;
use tracing::Instrument;
use url::Url;

use edo_core::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo_core::environment::Environment;
use edo_core::source::{SourceImpl, SourceResult};
use edo_core::storage::ConfigBuilder;
use edo_core::storage::{
    Artifact, ArtifactBuilder, Compression, Id, IdBuilder, MediaType, Storage,
};

/// A RemoteSource is rather simple
/// it is responsible for fetching a remote file and storing it as an
/// artifact
pub struct RemoteSource {
    url: Url,
    digest: String,
    out: PathBuf,
    is_archive: bool,
}

#[async_trait]
impl FromNode for RemoteSource {
    type Error = error::RemoteSourceError;

    async fn from_node(
        _: &Addr,
        node: &Node,
        _: &Context,
    ) -> Result<Self, error::RemoteSourceError> {
        node.validate_keys(&["url", "out", "ref"])?;
        let url = node
            .get("url")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "url",
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
            .and_then(|x| x.as_bool())
            .unwrap_or_default();
        let digest = node
            .get("ref")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "ref",
                type_: "string",
            })?;
        Ok(Self {
            url: Url::parse(&url).context(error::UrlSnafu)?,
            out: PathBuf::from(out),
            is_archive,
            digest,
        })
    }
}

non_configurable!(RemoteSource, error::RemoteSourceError);

#[async_trait]
impl SourceImpl for RemoteSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        let id = IdBuilder::default()
            .name(self.url.path().to_string())
            .digest(self.digest.clone())
            .version(None)
            .build()
            .context(error::IdSnafu)?;
        trace!(component = "source", type = "remote", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, _log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        trace!(component = "source", type = "remote", "fetching remote file from {}", self.url);
        let url = self.url.clone();
        async move {
            let client = reqwest::Client::new();
            let response = client
                .get(url.clone())
                .send()
                .await
                .context(error::RequestSnafu)?;
            ensure!(
                response.status().is_success(),
                error::FailedSnafu {
                    url: url.clone(),
                    message: response.text().await.context(error::RequestSnafu)?
                }
            );
            // Now we create a stream reader over the body
            let mut reader =
                StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));

            let mut artifact = ArtifactBuilder::default()
                .config(
                    ConfigBuilder::default()
                        .id(id.clone())
                        .metadata(json!({
                            "source": url.clone().to_string()
                        }))
                        .build()
                        .unwrap(),
                )
                .media_type(MediaType::Manifest)
                .build()
                .context(error::ArtifactSnafu)?;

            // Remote sources are stored in a single layer of the artifact
            let mut writer = storage.safe_start_layer().await?;
            tokio::io::copy(&mut reader, &mut writer)
                .await
                .context(error::IoSnafu)?;
            let layer = storage
                .safe_finish_layer(&MediaType::File(Compression::None), None, &writer)
                .await?;
            artifact.layers_mut().push(layer.clone());

            storage.safe_save(&artifact).await?;

            ensure!(
                layer.clone().digest().digest() == *id.digest(),
                error::DigestSnafu {
                    actual: layer.digest().digest(),
                    expected: id.digest()
                }
            );
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "fetching",
            id = id_s,
            url = self.url.clone().to_string(),
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
        // Staging is rather simple as we just want to move the remote file to the expected location
        let id = self.get_unique_id().await?;
        let out = path.join(self.out.clone());
        let artifact = storage.safe_open(&id).await?;
        let layer = artifact.layers().first().unwrap();
        let reader = storage.safe_read(layer).await?;
        if self.is_archive {
            trace!(component = "source", type = "remote", "staging contents of archive into {}", out.display());
            env.unpack(&out, reader).await?;
        } else {
            trace!(component = "source", type = "remote", "staging file to {}", out.display());
            env.write(&out, reader).await?;
        }
        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;

    use edo_core::{
        plugin::error::PluginError,
        source::SourceError,
        storage::{ArtifactBuilderError, IdBuilderError},
    };

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum RemoteSourceError {
        #[snafu(display("failed to create artifact manifest: {source}"))]
        Artifact { source: ArtifactBuilderError },
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(edo_core::context::ContextError, Box::new)))]
            source: Box<edo_core::context::ContextError>,
        },
        #[snafu(display("failed to fetch remote source from '{url}': {message}"))]
        Failed { url: url::Url, message: String },
        #[snafu(display("remote source has hash '{actual}' instead of expected '{expected}'"))]
        Digest { actual: String, expected: String },
        #[snafu(display("remote source definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
        #[snafu(display("failed to create artifact id: {source}"))]
        Id { source: IdBuilderError },
        #[snafu(display("io error occured during remote source fetch: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to make request to remote: {source}"))]
        Request { source: reqwest::Error },
        #[snafu(display("invalid url provided to remote source: {source}"))]
        Url { source: url::ParseError },
    }

    impl From<RemoteSourceError> for SourceError {
        fn from(value: RemoteSourceError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<RemoteSourceError> for PluginError {
        fn from(value: RemoteSourceError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
