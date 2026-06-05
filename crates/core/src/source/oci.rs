use async_trait::async_trait;
use edo::record;
use ocilot::{index::Index, models::Platform, uri::Uri};
use snafu::ResultExt;
use snafu::ensure;
use std::collections::BTreeSet;

use edo::context::{Context, Element, FromElement, Log};
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};

/// A OCI Image source is used to fetch
/// an oci image to use as a container image
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ImageSource {
    uri: String,
    #[serde(rename = "ref")]
    digest: String,
    platform: Option<Platform>,
}

#[async_trait]
impl FromElement for ImageSource {
    type Error = error::ImageSourceError;

    async fn new(
        element: &Element,
        _: &Context,
    ) -> std::result::Result<Self, error::ImageSourceError> {
        element.get().map_err(|e| e.into())
    }
}

#[async_trait]
impl SourceImpl for ImageSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        let id = Id::builder()
            .name(self.uri.clone())
            .digest(self.digest.clone())
            .build();
        trace!(subsystem = "source", component = "oci", id = %id, "calculated id");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let uri = Uri::new(&self.uri).await.context(error::OciSnafu)?;
        info!(
            subsystem = "source",
            component = "oci",
            op = "fetch",
            id = %id,
            uri = %self.uri,
            "pulling oci image {}",
            self.uri
        );

        // We do something rather clever for oci images, as we are going to one to one map the layers
        // and then handle staging as a filesystem ourself
        let index = Index::fetch(&uri).await.context(error::OciSnafu)?;
        // The actual digest that should be used, should be a merkle digest of the manifests
        let mut hasher = blake3::Hasher::new();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let hash_bytes = hasher.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        ensure!(
            *id.digest() == digest,
            error::DigestSnafu {
                actual: id.digest().clone(),
                expected: digest.clone()
            }
        );

        // We use ocilot to create a oci tarball for this imag
        let mut artifact = Artifact::builder()
            .config(
                Config::builder()
                    .id(id)
                    .provides(BTreeSet::from_iter([self.uri.to_string()]))
                    .build(),
            )
            .media_type(MediaType::Manifest)
            .build();

        let writer = storage.safe_start_layer().await?;
        let platform = self.platform.clone().unwrap_or_default();
        record!(
            log,
            "pull",
            "fetching oci archive for image at {} for platform {}",
            uri,
            platform
        );
        index
            .to_oci(&uri, Some(platform.clone()), writer.clone())
            .await
            .context(error::OciSnafu)?;
        let layer = storage
            .safe_finish_layer(
                &writer,
                &LayerOptions::builder()
                    .media_type(MediaType::Oci(Compression::None))
                    .platform(platform.clone())
                    .build(),
            )
            .await?;
        artifact.layers_mut().push(layer);
        storage.safe_save(&artifact).await?;
        Ok(artifact.clone())
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
    pub enum ImageSourceError {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(edo::context::ContextError, Box::new)))]
            source: Box<edo::context::ContextError>,
        },
        #[snafu(display("image has digest '{actual}' when expecting '{expected}"))]
        Digest { actual: String, expected: String },
        #[snafu(display("image source oci error: {source}"))]
        Oci { source: ocilot::error::Error },
        #[snafu(display("invalid image source at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("io error occured in image source: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to serialize image configuration: {source}"))]
        Serialize { source: serde_json::Error },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
    }

    impl From<ImageSourceError> for SourceError {
        fn from(value: ImageSourceError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<ImageSourceError> for ContextError {
        fn from(value: ImageSourceError) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
