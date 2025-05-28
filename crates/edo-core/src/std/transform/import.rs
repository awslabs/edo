use crate::context::{non_configurable, Addr, Context, FromNode, Handle, Log, Node};
use crate::environment::Environment;
use crate::source::Source;
use crate::storage::{ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, MediaType};
use crate::transform::{TransformImpl, TransformResult, TransformStatus};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::path::Path;

pub struct ImportTransform {
    pub addr: Addr,
    pub sources: IndexMap<String, Source>,
}

#[async_trait]
impl FromNode for ImportTransform {
    type Error = error::Error;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self, error::Error> {
        Ok(Self {
            addr: addr.clone(),
            sources: super::parse_sources(addr, node, ctx, |field, type_| error::Error::Field {
                field: field.to_string(),
                type_: type_.to_string(),
            })
            .await?,
        })
    }
}

non_configurable!(ImportTransform, error::Error);

#[async_trait]
impl TransformImpl for ImportTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        let addr = Addr::parse("//default")?;
        Ok(addr)
    }

    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        let mut hash = blake3::Hasher::new();
        for source in self.sources.values() {
            hash.update(source.get_unique_id().await?.digest().as_bytes());
        }
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        let id = IdBuilder::default()
            .name(
                self.addr
                    .to_string()
                    .strip_prefix("//")
                    .unwrap()
                    .to_string(),
            )
            .digest(digest)
            .version(None)
            .build()
            .unwrap();
        trace!(component = "transform", type = "import", "calculated id to be {id}");
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::new())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        for (addr, source) in self.sources.iter() {
            trace!(component = "transform", type = "import", "fetching source {addr}");
            source.fetch(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Create the output directory
        env.create_dir(Path::new("output")).await?;

        // Stage all the sources in the output directory
        for (addr, source) in self.sources.iter() {
            trace!(component = "transform", type = "import", "staging source {addr}");
            source
                .stage(log, ctx.storage(), env, Path::new("output"))
                .await?;
        }

        Ok(())
    }

    async fn transform(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        // Transform is simply as we just archive the output directory
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let mut artifact = ArtifactBuilder::default()
                .config(ConfigBuilder::default().id(id.clone()).build().unwrap())
                .media_type(MediaType::Manifest)
                .build()
                .unwrap();
            let writer = ctx.storage().safe_start_layer().await?;
            env.read(Path::new("output"), writer.clone()).await?;
            artifact.layers_mut().push(
                ctx.storage()
                    .safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer)
                    .await?,
            );
            ctx.storage().safe_save(&artifact).await?;
            Ok(artifact)
        }
        .await
        {
            Ok(e) => TransformStatus::Success(e),
            Err(e) => TransformStatus::Retryable(None, e),
        }
    }

    fn can_shell(&self) -> bool {
        false
    }

    fn shell(&self, _env: &Environment) -> TransformResult<()> {
        Ok(())
    }
}

pub mod error {
    use crate::storage::StorageError;
    use snafu::Snafu;

    use crate::{context::ContextError, plugin::error::PluginError, transform::TransformError};

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display(
            "import transform definitions require a field '{field}' with type '{type_}'"
        ))]
        Field { field: String, type_: String },
        #[snafu(transparent)]
        Storage {
            #[snafu(source(from(StorageError, Box::new)))]
            source: Box<StorageError>,
        },
    }

    impl From<Error> for TransformError {
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
