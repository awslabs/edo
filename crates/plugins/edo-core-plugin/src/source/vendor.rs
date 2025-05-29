use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf, absolute};

use async_trait::async_trait;
use edo_core::context::{Addr, Context, FromNode, Log, Node, non_configurable};
use edo_core::environment::Environment;
use edo_core::source::{SourceImpl, SourceResult};
use edo_core::storage::{
    Artifact, ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, MediaType, Storage,
};
use edo_core::util::{cmd_noinput, cmd_pipeout, copy_r};
use merkle_hash::MerkleTree;
use snafu::{OptionExt, ResultExt};
use tempfile::TempDir;
use tokio::fs::create_dir_all;
use tokio::io::AsyncWriteExt;
use tokio_tar::Builder;
use tracing::Instrument;
use which::which;

pub struct VendorSource {
    path: PathBuf,
    inside: PathBuf,
    out: PathBuf,
    rust: bool,
    go: Vec<PathBuf>,
}

#[async_trait]
impl FromNode for VendorSource {
    type Error = error::VendorError;

    async fn from_node(_: &Addr, node: &Node, _: &Context) -> Result<Self, error::VendorError> {
        node.validate_keys(&["path", "inside", "out"])?;
        let path = node
            .get("path")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "path",
                type_: "string",
            })?;
        let inside = node
            .get("inside")
            .unwrap()
            .as_string()
            .context(error::FieldSnafu {
                field: "inside",
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
        let rust = node
            .get("rust")
            .and_then(|x| x.as_bool())
            .unwrap_or_default();
        let go = if let Some(list) = node.get("go") {
            let list = list.as_list().context(error::FieldSnafu {
                field: "go",
                type_: "list of strings",
            })?;
            let mut paths = Vec::new();
            for entry in list {
                paths.push(PathBuf::from(entry.as_string().context(
                    error::FieldSnafu {
                        field: "go",
                        type_: "list of strings",
                    },
                )?));
            }
            paths
        } else {
            Vec::default()
        };
        Ok(Self {
            path: PathBuf::from(path),
            out: PathBuf::from(out),
            inside: PathBuf::from(inside),
            rust,
            go,
        })
    }
}

non_configurable!(VendorSource, error::VendorError);

#[async_trait]
impl SourceImpl for VendorSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // The digest should be calculated as a merkle hash of the source files
        let apath = absolute(&self.path).context(error::IoSnafu)?;
        let merkle = MerkleTree::builder(apath.to_string_lossy().as_ref())
            .build()
            .context(error::MerkleSnafu)?;
        let hash = merkle.root.item.hash;
        // Local files will never be precached usually
        let digest = base16::encode_lower(hash.as_slice());

        Ok(IdBuilder::default()
            .name(
                self.path
                    .file_name()
                    .unwrap_or(self.path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
            )
            .version(None)
            .digest(digest)
            .build()
            .unwrap())
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        // First we want to create a temporary directory and copy the source to it
        let temp = TempDir::new().context(error::IoSnafu)?;
        let tmp_dir = temp.path();
        copy_r(&self.path, tmp_dir).await.context(error::IoSnafu)?;
        if self.rust {
            trace!(component = "source", type = "vendor", "vendoring rust dependencies");
            let work_dir = tmp_dir.join(self.inside.clone());
            async move {
                let cargo = which("cargo").context(error::CargoNotFoundSnafu)?;
                let target_dir = work_dir.join(".cargo");
                if !target_dir.exists() {
                    create_dir_all(&target_dir)
                        .await
                        .context(error::IoSnafu)?;
                }
                let target_file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(target_dir.join("config.toml"))
                    .context(error::IoSnafu)?;

                cmd_pipeout(
                    &work_dir,
                    log,
                    target_file,
                    cargo,
                    ["vendor"],
                    &HashMap::new(),
                ).context(error::IoSnafu)?;
                Ok::<(), error::VendorError>(())
            }
            .instrument(
                info_span!(target: "vendor-source", "vendoring cargo dependencies", id = id_s, log = log.log_name()),
            )
            .await?;
        }
        if !self.go.is_empty() {
            trace!(component = "source", type = "vendor", "vendoring go dependencies");
            async move {
                let workspace = tmp_dir.join(self.inside.clone());
                for pkg_dir in self.go.iter() {
                    let go = which("go").context(error::GoNotFoundSnafu)?;
                    let work_dir = workspace.join(pkg_dir);
                    cmd_noinput(
                        &work_dir,
                        log,
                        go,
                        ["mod", "vendor"],
                        &HashMap::new(),
                    ).context(error::IoSnafu)?;
                }
                Ok::<(), error::VendorError>(())
            }
            .instrument(info_span!(target: "vendor-source", "vendoring go packages", id = id_s, log = log.log_name()))
            .await?;
        }

        // Build the artifact manifest
        let mut artifact = ArtifactBuilder::default()
            .config(ConfigBuilder::default().id(id.clone()).build().unwrap())
            .media_type(MediaType::Manifest)
            .build()
            .unwrap();

        // Create a layer for the resulting archive
        let mut writer = storage.safe_start_layer().await?;
        // We want to archive it if its a directory
        let mut archive = Builder::new(writer.clone());
        archive
            .append_dir_all(".", temp.path())
            .await
            .context(error::IoSnafu)?;
        archive.finish().await.context(error::IoSnafu)?;
        writer.flush().await.context(error::IoSnafu)?;
        artifact.layers_mut().push(
            storage
                .safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer)
                .await?,
        );

        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }

    async fn stage(
        &self,
        _log: &Log,
        storage: &Storage,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()> {
        // Staging is rather simple as we just want to move the remote file to the expected location
        let out = path.join(self.out.clone());
        let id = self.get_unique_id().await?;
        let artifact = storage.safe_open(&id).await?;
        let layer = artifact.layers().first().unwrap();
        let reader = storage.safe_read(layer).await?;
        trace!(component = "source", type = "vendor", "staging contents to {}", out.display());
        env.unpack(&out, reader).await?;

        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;

    use edo_core::{context::ContextError, plugin::error::PluginError, source::SourceError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum VendorError {
        #[snafu(display("vendor source cannot vend rust dependencies without cargo: {source}"))]
        CargoNotFound { source: which::Error },
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display(
            "vendor source definition requires a field '{field}' with type '{type_}'"
        ))]
        Field { field: String, type_: String },
        #[snafu(display("vendor source cannot vend go dependencies without go: {source}"))]
        GoNotFound { source: which::Error },
        #[snafu(display("io error occured while handling vendor source: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to calculate merkle hash of source: {source}"))]
        Merkle {
            source: merkle_hash::error::IndexingError,
        },
    }

    impl From<VendorError> for SourceError {
        fn from(value: VendorError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<VendorError> for PluginError {
        fn from(value: VendorError) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
