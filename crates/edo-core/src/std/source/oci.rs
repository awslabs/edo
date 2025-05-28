use async_trait::async_trait;
use edo_oci::{index::Index, models::Platform, uri::Uri};
use futures::future::try_join_all;
use snafu::ensure;
use snafu::{OptionExt, ResultExt};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use tokio::task::JoinError;
use tracing::Instrument;

use crate::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use crate::environment::Environment;
use crate::source::{SourceImpl, SourceResult};
use crate::storage::{
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
        let id_s = id.to_string();
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
                expected: self.digest.clone()
            }
        );
        let image = index
            .fetch_image(&self.uri, Some(self.platform.clone()))
            .await
            .context(error::OciSnafu)?
            .unwrap();
        // Now here comes the fun part we are going to create parallel tasks for each layer in this image
        let mut handles = Vec::new();
        let image_config = image
            .fetch_config(&self.uri)
            .await
            .context(error::OciSnafu)?;
        let image_metadata = serde_json::to_value(&image_config).context(error::SerializeSnafu)?;
        let mut artifact = ArtifactBuilder::default()
            .config(
                ConfigBuilder::default()
                    .id(id)
                    .provides(BTreeSet::from_iter([self.uri.to_string()]))
                    .metadata(image_metadata)
                    .build()
                    .context(error::ConfigSnafu)?,
            )
            .media_type(MediaType::Manifest)
            .build()
            .context(error::ArtifactSnafu)?;
        for layer in image.layers() {
            let uri = self.uri.clone();
            let storage = storage.clone();
            let digest = layer.digest().to_string();
            let layer = layer.clone();
            handles.push(tokio::spawn(
                async move {
                    let mut reader = layer.open(&uri).await.context(error::OciSnafu)?;
                    let mut writer = storage.safe_start_layer().await?;
                    tokio::io::copy(&mut reader, &mut writer)
                        .await
                        .context(error::IoSnafu)?;
                    let artifact_layer = storage
                        .safe_finish_layer(
                            &MediaType::File(Compression::None),
                            layer.platform(),
                            &writer,
                        )
                        .await?;
                    Ok::<Layer, error::ImageSourceError>(artifact_layer)
                }
                .instrument(info_span!("download", blob = digest, id = id_s)),
            ));
        }
        let layers = wait(handles).await?;
        *artifact.layers_mut() = layers;
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

async fn wait<I, R>(handles: I) -> Result<Vec<R>, error::ImageSourceError>
where
    R: Clone,
    I: IntoIterator,
    I::Item: Future<Output = std::result::Result<Result<R, error::ImageSourceError>, JoinError>>,
{
    let result = try_join_all(handles).await;
    let mut success = Vec::new();
    let mut failures = Vec::new();
    for entry in result.context(error::JoinSnafu)? {
        match entry {
            Ok(result) => success.push(result),
            Err(e) => failures.push(e),
        }
    }
    if !failures.is_empty() {
        error::ChildSnafu { failures }.fail()
    } else {
        Ok(success)
    }
}

pub mod error {
    use snafu::Snafu;

    use crate::{plugin::error::PluginError, source::SourceError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum ImageSourceError {
        #[snafu(display("failed to make artifact manifest: {source}"))]
        Artifact {
            source: crate::storage::ArtifactBuilderError,
        },
        #[snafu(display("{}", failures.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n")))]
        Child { failures: Vec<ImageSourceError> },
        #[snafu(display("failed to make artifact config: {source}"))]
        Config {
            source: crate::storage::ConfigBuilderError,
        },
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(crate::context::ContextError, Box::new)))]
            source: Box<crate::context::ContextError>,
        },
        #[snafu(display("image has digest '{actual}' when expecting '{expected}"))]
        Digest { actual: String, expected: String },
        #[snafu(display("failed to wait on parallel task: {source}"))]
        Join { source: tokio::task::JoinError },
        #[snafu(display("image source oci error: {source}"))]
        Oci { source: edo_oci::error::Error },
        #[snafu(display("image source definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
        #[snafu(display("failed to make id: {source}"))]
        Id {
            source: crate::storage::IdBuilderError,
        },
        #[snafu(display("io error occured in image source: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to serialize image configuration: {source}"))]
        Serialize { source: serde_json::Error },
        #[snafu(transparent)]
        Storage {
            source: crate::storage::StorageError,
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
