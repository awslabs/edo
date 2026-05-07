use async_trait::async_trait;
use edo::record;
use futures::TryStreamExt;
use serde_json::json;
use snafu::{OptionExt, ResultExt, ensure};
use std::path::Path;
use std::path::PathBuf;
use tokio_util::io::StreamReader;
use tracing::Instrument;
use url::Url;

use edo::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo::environment::Environment;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, MediaType, Storage};

/// A source that fetches a file from a remote URL and stores it as an artifact.
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
        let id = Id::builder()
            .name(self.url.path().to_string())
            .digest(self.digest.clone())
            .build();
        trace!(component = "source", type = "remote", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        trace!(component = "source", type = "remote", "fetching remote file from {}", self.url);
        let url = self.url.clone();
        async move {
            record!(log, "fetch", "fetching artifact from {url}");
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

            let mut artifact = Artifact::builder()
                .config(
                    Config::builder()
                        .id(id.clone())
                        .metadata(json!({
                            "source": url.clone().to_string()
                        }))
                        .build(),
                )
                .media_type(MediaType::Manifest)
                .build();

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
        log: &Log,
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
            record!(log, "unpack", "extracting archive into {out:?}");
            env.unpack(&out, reader).await?;
        } else {
            trace!(component = "source", type = "remote", "staging file to {}", out.display());
            record!(log, "copy", "copying file to {out:?}");
            env.write(&out, reader).await?;
        }
        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;

    use edo::{context::error::ContextError, source::SourceError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum RemoteSourceError {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(edo::context::ContextError, Box::new)))]
            source: Box<edo::context::ContextError>,
        },
        #[snafu(display("failed to fetch remote source from '{url}': {message}"))]
        Failed { url: url::Url, message: String },
        #[snafu(display("remote source has hash '{actual}' instead of expected '{expected}'"))]
        Digest { actual: String, expected: String },
        #[snafu(display("remote source definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
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

    impl From<RemoteSourceError> for ContextError {
        fn from(value: RemoteSourceError) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
