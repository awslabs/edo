use async_trait::async_trait;
use edo::record;
use edo::storage::{Layer, LayerOptions};
use futures::TryStreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
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

impl RemoteSource {
    /// Returns the bare hex digest portion of the user-supplied content
    /// reference, used as both the layer digest and the `has_local_blob`
    /// lookup key. Catalogs strip the `sha256:` prefix consistently;
    /// match that convention here.
    fn blob_digest(&self) -> &str {
        self.digest
            .strip_prefix("sha256:")
            .unwrap_or(self.digest.as_str())
    }
}

#[async_trait]
impl SourceImpl for RemoteSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // Hash the user-supplied content digest together with `out` so a
        // change to `out` invalidates the cached *manifest*, even though
        // the blob itself is still content-addressed by `self.digest`.
        let mut hasher = Sha256::new();
        hasher.update(self.digest.as_bytes());
        hasher.update(
            self.out
                .as_ref()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        let manifest_digest = base16::encode_lower(hasher.finalize().as_slice());
        let id = Id::builder()
            .name(self.url.path().to_string())
            .digest(manifest_digest)
            .build();
        trace!(subsystem = "source", component = "remote", id = %id, "calculated id");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        info!(
            subsystem = "source",
            component = "remote",
            op = "fetch",
            id = %id,
            url = %self.url,
            "fetching {}",
            self.url
        );
        let url = self.url.clone();
        let blob_digest = self.blob_digest().to_string();
        async move {
            // Build the manifest skeleton once; we'll fill in the layer
            // either from a fresh download or by reusing an existing blob.
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
            // Determine the mediatype from the url's filename component
            // (ignoring query/fragment which would defeat the suffix-anchored regexes).
            let filename = url
                .path_segments()
                .and_then(|segments| segments.last())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| url.path());
            let media_type = MediaType::detect(filename)?;

            // Short-circuit: if the local cache already has the blob the
            // user asked for (e.g. `out` changed but the URL/digest did
            // not), reuse it instead of re-downloading. The manifest
            // `Id` is the only thing that changed.
            let layer = if storage.has_local_blob(&blob_digest).await? {
                trace!(
                    subsystem = "source",
                    component = "remote",
                    op = "blob-reuse",
                    digest = %blob_digest,
                    "reusing existing local blob"
                );
                record!(log, "reuse", "reusing already-cached blob {}", blob_digest);
                // Read the actual blob size off disk so the persisted
                // manifest accurately describes the layer. Anything else
                // would lie about a content-addressed property and break
                // any future consumer (e.g. an S3 mirror that range-reads
                // by `Layer::size`).
                let size = storage.local_blob_size(&blob_digest).await?.unwrap_or(0);
                Layer::builder()
                    .media_type(media_type)
                    .digest(blob_digest.clone())
                    .size(size as usize)
                    .build()
            } else {
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

                // Remote sources are stored in a single layer of the artifact
                let mut writer = storage.safe_start_layer().await?;
                tokio::io::copy(&mut reader, &mut writer)
                    .await
                    .context(error::IoSnafu)?;
                let layer = storage
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder().media_type(media_type).build(),
                    )
                    .await?;

                // The remote source contract requires the blob's digest
                // match the user-supplied `ref`. Compare against
                // `self.digest` directly — the manifest `Id`'s digest is
                // now `sha256(ref || out)` so it must not be used here.
                ensure!(
                    layer.digest().digest() == blob_digest,
                    error::DigestSnafu {
                        actual: layer.digest().digest(),
                        expected: blob_digest.clone()
                    }
                );
                layer
            };

            // Record `out` (if any) at the artifact level, keyed by the
            // freshly-attached layer's digest.
            if let Some(hint) = self.out.clone() {
                artifact
                    .config_mut()
                    .path_hints_mut()
                    .insert(layer.digest().digest(), hint);
            }
            artifact.layers_mut().push(layer);

            storage.safe_save(&artifact).await?;
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "source-fetch",
            subsystem = "source",
            component = "remote",
            id = %id_s,
            url = %self.url
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
