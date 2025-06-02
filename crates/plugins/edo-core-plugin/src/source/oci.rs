use async_trait::async_trait;
use futures::future::try_join_all;
use ocilot::{index::Index, models::Platform, uri::Uri};
use snafu::ensure;
use snafu::{OptionExt, ResultExt};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use tokio::task::JoinError;
use tracing::Instrument;

use edo_core::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo_core::environment::Environment;
use edo_core::source::{SourceImpl, SourceResult};
use edo_core::storage::{
    Artifact, ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, Layer, MediaType, Storage,
};

/// A OCI Image source is used to fetch
/// an oci image to use as a container image
pub struct ImageSource {
    uri: Uri,
    digest: String,
    platform: Platform,
}

#[async_trait]
impl FromNode for ImageSource {
    type Error = error::ImageSourceError;

    async fn from_node(
        _: &Addr,
        node: &Node,
        _: &Context,
    ) -> std::result::Result<Self, error::ImageSourceError> {
        node.validate_keys(&["url", "ref"])?;
        let url = node
            .get("url")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "url",
                type_: "string",
            })?;
        let platform = node
            .get("platform")
            .and_then(|x| x.as_string())
            .map(|x| Platform::from(x.clone()))
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
            uri: Uri::new(&url).await.context(error::OciSnafu)?,
            platform,
            digest,
        })
    }
}

non_configurable!(ImageSource, error::ImageSourceError);

/// A OCI Filesystem source is used to fetch
/// an oci artifact or image using ocilot as a filesystem archive

#[async_trait]
impl SourceImpl for ImageSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        let id = IdBuilder::default()
            .name(self.uri.to_string())
            .digest(self.digest.clone())
            .version(None)
            .build()
            .context(error::IdSnafu)?;
        trace!(component = "source", type = "oci", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, _log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        trace!(component = "source", type = "oci", "pulling oci image from {}", self.uri);

        // We do something rather clever for oci images, as we are going to one to one map the layers
        // and then handle staging as a filesystem ourself
        let index = Index::fetch(&self.uri).await.context(error::OciSnafu)?;
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
        let mut artifact = ArtifactBuilder::default()
            .config(
                ConfigBuilder::default()
                    .id(id)
                    .provides(BTreeSet::from_iter([self.uri.to_string()]))
                    .build()
                    .context(error::ConfigSnafu)?,
            )
            .media_type(MediaType::Manifest)
            .build()
            .context(error::ArtifactSnafu)?;

        let writer = storage.safe_start_layer().await?;
        index
            .to_oci(&self.uri, Some(self.platform.clone()), writer.clone())
            .await
            .context(error::OciSnafu)?;
        let layer = storage
            .safe_finish_layer(
                &MediaType::Oci(Compression::None),
                Some(self.platform.clone()),
                &writer,
            )
            .await?;
        artifact.layers_mut().push(layer);
        storage.safe_save(&artifact).await?;
        Ok(artifact.clone())
    }

    async fn stage(
        &self,
        _log: &Log,
        _storage: &Storage,
        _env: &Environment,
        _path: &Path,
    ) -> SourceResult<()> {
        // An oci image does not get staged at all
        // TODO: Implement the parallel extract here
        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;

    use edo_core::{plugin::error::PluginError, source::SourceError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum ImageSourceError {
        #[snafu(display("failed to make artifact manifest: {source}"))]
        Artifact {
            source: edo_core::storage::ArtifactBuilderError,
        },
        #[snafu(display("failed to make artifact config: {source}"))]
        Config {
            source: edo_core::storage::ConfigBuilderError,
        },
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(edo_core::context::ContextError, Box::new)))]
            source: Box<edo_core::context::ContextError>,
        },
        #[snafu(display("image has digest '{actual}' when expecting '{expected}"))]
        Digest { actual: String, expected: String },
        #[snafu(display("image source oci error: {source}"))]
        Oci { source: ocilot::error::Error },
        #[snafu(display("image source definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
        #[snafu(display("failed to make id: {source}"))]
        Id {
            source: edo_core::storage::IdBuilderError,
        },
        #[snafu(display("io error occured in image source: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to serialize image configuration: {source}"))]
        Serialize { source: serde_json::Error },
        #[snafu(transparent)]
        Storage {
            source: edo_core::storage::StorageError,
        },
    }

    impl From<ImageSourceError> for SourceError {
        fn from(value: ImageSourceError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<ImageSourceError> for PluginError {
        fn from(value: ImageSourceError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
