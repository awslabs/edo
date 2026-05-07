use async_trait::async_trait;
use edo::record;
use ocilot::{index::Index, models::Platform, uri::Uri};
use snafu::ensure;
use snafu::{OptionExt, ResultExt};
use std::collections::BTreeSet;
use std::path::Path;

use edo::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo::environment::Environment;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, MediaType, Storage};

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
        let id = Id::builder()
            .name(self.uri.to_string())
            .digest(self.digest.clone())
            .build();
        trace!(component = "source", type = "oci", "calculated id to be {id}");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
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
        record!(
            log,
            "pull",
            "fetching oci archive for image at {} for platform {}",
            self.uri,
            self.platform
        );
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

    use edo::{context::error::ContextError, source::SourceError};

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
        #[snafu(display("image source definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
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
