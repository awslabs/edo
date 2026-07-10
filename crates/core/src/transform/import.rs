use async_trait::async_trait;
use edo::context::{Addr, Context, Element, FromElement, Handle, Log};
use edo::environment::Environment;
use edo::source::Source;
use edo::storage::{
    Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType,
};
use edo::transform::{TransformImpl, TransformResult, TransformStatus};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use snafu::OptionExt;
use std::path::Path;

/// A transform that imports sources directly into the build environment as an artifact.
pub struct ImportTransform {
    pub addr: Addr,
    pub sources: IndexMap<String, Vec<Source>>,
}

#[async_trait]
impl FromElement for ImportTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut sources = IndexMap::new();
        for (scope, source_list) in element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .context(error::NoSourceSnafu {
                addr: element.addr.clone(),
            })?
        {
            let mut entries = Vec::new();
            for element in source_list.iter() {
                entries.push(ctx.add_source(element).await?);
            }
            sources.insert(scope.clone(), entries);
        }
        Ok(Self {
            addr: element.addr.clone(),
            sources,
        })
    }
}

#[async_trait]
impl TransformImpl for ImportTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        let addr = Addr::parse("//default")?;
        Ok(addr)
    }

    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        let mut hash = Sha256::new();
        for source_list in self.sources.values() {
            for source in source_list {
                hash.update(source.get_unique_id().await?.digest().as_bytes());
            }
        }
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_slice());
        let id = Id::builder()
            .name(
                self.addr
                    .to_string()
                    .strip_prefix("//")
                    .unwrap()
                    .to_string(),
            )
            .digest(digest)
            .build();
        trace!(subsystem = "transform", component = "import", id = %id, "calculated id");
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::new())
    }

    /// Short-circuits prepare when every input source is already cached.
    /// Import transforms only fetch sources in `prepare`, so the call is
    /// pure overhead when everything is already on disk.
    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        for source_list in self.sources.values() {
            for source in source_list {
                if !source.is_cached(ctx.storage()).await? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        for (addr, source_list) in self.sources.iter() {
            trace!(
                subsystem = "transform",
                component = "import",
                op = "fetch",
                addr = %addr,
                "fetching source"
            );
            for source in source_list {
                source.cache(log, ctx.storage()).await?;
            }
        }
        Ok(())
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Create the output directory
        let output = Path::new("output");
        env.create_dir(output).await?;

        // Stage all the sources in the output directory
        for (addr, source_list) in self.sources.iter() {
            trace!(
                subsystem = "transform",
                component = "import",
                op = "stage",
                addr = %addr,
                "staging source"
            );
            for source in source_list {
                let id = source.get_unique_id().await?;
                env.stage(
                    ctx,
                    ArtifactStageOptions::builder().id(id).path(output).build(),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn transform(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        // Transform is simply as we just archive the output directory
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(Path::new("output"), writer.clone()).await?;
            artifact.layers_mut().push(
                ctx.storage()
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder()
                            .media_type(MediaType::Tar(Compression::None))
                            .build(),
                    )
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
    use edo::context::Addr;
    use edo::storage::StorageError;
    use edo::{context::ContextError, transform::TransformError};
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("no source provided to import transform at {addr}"))]
        NoSource { addr: Addr },
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

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
