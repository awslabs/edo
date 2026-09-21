use async_trait::async_trait;
use futures::TryStreamExt;
use serde_json::json;
use snafu::{ResultExt, ensure};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::io::StreamReader;
use tracing::Instrument;
use url::Url;

use edo::{
    context::{Context, Element, FromElement, Log},
    record,
    source::{SourceImpl, SourceResult},
    storage::{Artifact, Config, Digest, Id, Layer, LayerOptions, MediaType, Storage},
};

/// A source that fetches a file from a remote URL and stores it as an artifact.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RemoteSource {
    url: Url,
    #[serde(rename = "ref")]
    digest: Digest,
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
        // Hash the user-supplied content digest together with `out` so a
        // change to `out` invalidates the cached *manifest*, even though
        // the blob itself is still content-addressed by `self.digest`.
        let mut digest = Digest::with_algorithm(self.digest.algorithm());
        digest.update(self.digest.hash());
        digest.update(
            self.out
                .as_ref()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        let id = Id::builder()
            .name(self.url.path().to_string())
            .digest(digest.build())
            .build();
        trace!(subsystem = "source", component = "remote", id = %id, "calculated id");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        let url = self.url.clone();
        let blob_digest = self.digest.clone();
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
                .and_then(|mut segments| segments.next_back())
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
                let response = send_with_ipv4_fallback(log, &url).await?;
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
                writer.set_algorithm(blob_digest.algorithm());
                tokio::io::copy(&mut reader, &mut writer)
                    .await
                    .context(error::IoSnafu)?;
                let layer = storage
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder().media_type(media_type).build(),
                    )
                    .await?;

                // Validate content against `ref`, not the manifest digest.
                ensure!(
                    *layer.digest() == blob_digest,
                    error::DigestSnafu {
                        actual: layer.digest().clone(),
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
                    .insert(layer.digest().clone(), hint);
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

/// Upper bound on establishing a TCP connection, so an unreachable address
/// fails instead of waiting for the OS TCP timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Build a reqwest client. When `ipv4_only`, bind 0.0.0.0 so only IPv4
/// remotes are dialed.
fn build_client(ipv4_only: bool) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("edo/", env!("CARGO_PKG_VERSION")))
        .referer(false)
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(CONNECT_TIMEOUT);
    if ipv4_only {
        builder = builder.local_address(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    }
    builder.build()
}

/// Whether the host of `url` resolves to both IPv4 and IPv6 addresses.
async fn host_is_dual_stack(url: &Url) -> bool {
    let Some(host) = url.host_str().map(str::to_owned) else {
        return false;
    };
    let port = url.port_or_known_default().unwrap_or(0);
    tokio::task::spawn_blocking(move || {
        let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
            return false;
        };
        let mut v4 = false;
        let mut v6 = false;
        for addr in addrs {
            if addr.is_ipv4() {
                v4 = true;
            } else if addr.is_ipv6() {
                v6 = true;
            }
            if v4 && v6 {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// Whether `err` was caused by an `ENETUNREACH`/`EHOSTUNREACH` socket error.
fn is_unreachable(err: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable
            )
        {
            return true;
        }
        source = e.source();
    }
    false
}

/// GET `url`. If the request fails with a network/host-unreachable error
/// and the host is dual-stack, retry once over IPv4 only.
async fn send_with_ipv4_fallback(
    log: &Log,
    url: &Url,
) -> Result<reqwest::Response, error::RemoteSourceError> {
    let client = build_client(false).context(error::RequestSnafu)?;
    match client.get(url.clone()).send().await {
        Ok(response) => Ok(response),
        Err(first) if is_unreachable(&first) && host_is_dual_stack(url).await => {
            debug!(
                subsystem = "source",
                component = "remote",
                url = %url,
                error = %first,
                "remote unreachable on dual-stack host, retrying with IPv4-only local bind"
            );
            record!(
                log,
                "fetch",
                "remote unreachable ({first}); retrying with IPv4-only"
            );
            let client = build_client(true).context(error::RequestSnafu)?;
            client
                .get(url.clone())
                .send()
                .await
                .context(error::RetrySnafu { first })
        }
        Err(err) => Err(err).context(error::RequestSnafu),
    }
}

pub mod error {
    use snafu::Snafu;

    use edo::{context::error::ContextError, source::SourceError, storage::Digest};

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
        Digest { actual: Digest, expected: Digest },
        #[snafu(display("io error occured during remote source fetch: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to make request to remote: {source}"))]
        Request { source: reqwest::Error },
        #[snafu(display(
            "failed to make request to remote: {first}; IPv4-only retry also failed: {source}"
        ))]
        Retry {
            first: reqwest::Error,
            source: reqwest::Error,
        },
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
