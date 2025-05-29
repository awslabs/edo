use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use async_trait::async_trait;
use edo_core::context::{Addr, Context, FromNode, Node};
use edo_core::non_configurable;
use edo_core::source::{SourceResult, VendorImpl};
use edo_core::storage::Artifact;
use edo_oci::index::Index;
use edo_oci::registry::Registry;
use edo_oci::repository::Repository;
use edo_oci::uri::{Reference, RegistryUri, UriBuilder};
use semver::{Version, VersionReq};
use snafu::{OptionExt, ResultExt, ensure};
use tokio::io::AsyncReadExt;

/// An Image vendor is a provider of oci images via some oci compliant registry
pub struct ImageVendor {
    registry: Registry,
}

unsafe impl Send for ImageVendor {}
unsafe impl Sync for ImageVendor {}

#[async_trait]
impl VendorImpl for ImageVendor {
    async fn get_options(&self, name: &str) -> SourceResult<HashSet<Version>> {
        let mut versions = HashSet::new();
        let repo = Repository::new(&self.registry, name);
        for tag in repo.tags().await.context(error::OciSnafu)? {
            let stag = if tag.starts_with("v") {
                tag.strip_prefix("v").unwrap()
            } else {
                tag.as_str()
            };
            // First check if the tag matches a version
            if let Ok(version) = Version::parse(stag) {
                versions.insert(version.clone());
            }
        }
        Ok(versions)
    }

    async fn resolve(&self, name: &str, version: &Version) -> SourceResult<Node> {
        let mut uri = UriBuilder::default()
            .registry(self.registry.clone())
            .repository(name)
            .reference(Reference::Tag(version.to_string()))
            .build()
            .unwrap();

        let mut index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        if index.is_none() {
            // Adjust the tag to have a 'v' prefix
            uri = UriBuilder::default()
                .registry(self.registry.clone())
                .repository(name)
                .reference(Reference::Tag(format!("v{version}")))
                .build()
                .unwrap();
            index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        }
        ensure!(
            index.is_some(),
            error::VendedSnafu {
                name,
                version: version.clone()
            }
        );
        let index = index.unwrap();
        // The actual digest that should be used, should be a merkle digest of the manifests
        let mut hasher = blake3::Hasher::new();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let hash_bytes = hasher.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        Ok(Node::new_definition(
            "source",
            "image",
            name,
            BTreeMap::from([
                ("url".to_string(), Node::new_string(uri.to_string())),
                ("ref".to_string(), Node::new_string(digest)),
            ]),
        ))
    }

    async fn get_dependencies(
        &self,
        name: &str,
        version: &Version,
    ) -> SourceResult<Option<HashMap<String, VersionReq>>> {
        let mut found: HashMap<String, VersionReq> = HashMap::new();
        if let Some(artifact) = self.get_artifact_config(name, version).await? {
            for (name, req) in artifact
                .config()
                .requires()
                .get("depends")
                .unwrap_or(&BTreeMap::new())
            {
                if let Some(require) = found.get_mut(name) {
                    for entry in req.comparators.iter() {
                        require.comparators.push(entry.clone());
                    }
                } else {
                    found.insert(name.clone(), req.clone());
                }
            }
        } else {
            return Ok(None);
        }
        Ok(Some(found))
    }
}

#[async_trait]
impl FromNode for ImageVendor {
    type Error = error::Error;

    async fn from_node(_addr: &Addr, node: &Node, _ctx: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["uri"])?;
        let uri = node
            .get("uri")
            .and_then(|x| x.as_string())
            .context(error::FieldSnafu {
                field: "uri",
                type_: "string",
            })?;
        let registry_uri = RegistryUri::from_str(uri.as_str()).context(error::OciSnafu)?;
        Ok(Self {
            registry: Registry::new(&registry_uri)
                .await
                .context(error::OciSnafu)?,
        })
    }
}

non_configurable!(ImageVendor, error::Error);

impl ImageVendor {
    async fn get_artifact_config(
        &self,
        name: &str,
        version: &Version,
    ) -> Result<Option<Artifact>, error::Error> {
        let mut uri = UriBuilder::default()
            .registry(self.registry.clone())
            .repository(name)
            .reference(Reference::Tag(version.to_string()))
            .build()
            .unwrap();

        let mut index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        if index.is_none() {
            // Adjust the tag to have a 'v' prefix
            uri = UriBuilder::default()
                .registry(self.registry.clone())
                .repository(name)
                .reference(Reference::Tag(format!("v{version}")))
                .build()
                .unwrap();
            index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        }
        if index.is_none() {
            return Ok(None);
        }
        let index = index.unwrap();
        if let Some(image) = index
            .fetch_image(&uri, None)
            .await
            .context(error::OciSnafu)?
        {
            // Check if this is an edo artifact, if it is we can read it
            let mut config = image.config().open(&uri).await.context(error::OciSnafu)?;
            let mut buffer = Vec::new();
            config
                .read_to_end(&mut buffer)
                .await
                .context(error::IoSnafu)?;
            if let Ok(artifact) = serde_json::from_slice(buffer.as_slice()) {
                // This is an artifact!
                return Ok(Some(artifact));
            }
        }
        Ok(None)
    }
}

pub mod error {
    use semver::Version;
    use snafu::Snafu;

    use edo_core::{context::ContextError, plugin::error::PluginError, source::SourceError};

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("oci vendor definition requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
        #[snafu(display("io error occured interacting with oci registry: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to interact with oci registry: {source}"))]
        Oci { source: edo_oci::error::Error },
        #[snafu(display("could not find an oci image matching {name}@{version}"))]
        Vended { name: String, version: Version },
    }

    impl From<Error> for SourceError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for PluginError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
