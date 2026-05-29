use async_trait::async_trait;
use edo::record;
use edo::storage::LayerOptions;
use futures::TryStreamExt;
use serde_json::json;
use snafu::{ResultExt, ensure};
use std::path::PathBuf;
use tokio_util::io::StreamReader;
use tracing::Instrument;
use url::Url;

use edo::context::{Context, Element, FromElement, Log};
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Config, Id, MediaType, Storage};

/// A source that fetches a file from a remote URL and stores it as an artifact.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RemoteSource {
    url: Url,
    #[serde(rename = "ref")]
    digest: String,
    out: Option<PathBuf>,
}

#[async_trait]
impl FromElement for RemoteSource {
    type Error = error::RemoteSourceError;

    async fn new(element: &Element, _: &Context) -> Result<Self, error::RemoteSourceError> {
        element.get().map_err(|e| e.into())
    }
}

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
            let client = reqwest::Client::builder()
                .user_agent(concat!("edo/", env!("CARGO_PKG_VERSION")))
                .referer(false)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .context(error::RequestSnafu)?;
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
            // Determine the mediatype from the url
            let media_type = MediaType::detect(url.as_str())?;
            let layer = storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(media_type)
                        .maybe_path_hint(self.out.clone())
                        .build(),
                )
                .await?;
            artifact.layers_mut().push(layer.clone());

            ensure!(
                layer.clone().digest().digest() == *id.digest(),
                error::DigestSnafu {
                    actual: layer.digest().digest(),
                    expected: id.digest()
                }
            );
            storage.safe_save(&artifact).await?;
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "fetching",
            id = id_s,
            url = self.url.clone().to_string(),
        ))
        .await
    }
}

pub mod error {
    use snafu::Snafu;

    use edo::{
        context::{Addr, error::ContextError},
        source::SourceError,
    };

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
        #[snafu(display("invalid remote source definition at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
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
