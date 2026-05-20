use async_trait::async_trait;
use edo::{
    context::{Addr, Context, FromNode, Handle, Log, Node, non_configurable},
    environment::Environment,
    storage::{Artifact, Compression, Config, Id, MediaType},
    transform::{TransformImpl, TransformResult, TransformStatus},
};
use snafu::OptionExt;
use std::path::Path;

/// A transform that composes multiple dependency artifacts into a single output artifact.
pub struct ComposeTransform {
    pub addr: Addr,
    pub arch: Option<String>,
    pub depends: Vec<Addr>,
}

#[async_trait]
impl FromNode for ComposeTransform {
    type Error = error::Error;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self, error::Error> {
        let depends = super::parse_depends(node, "depends", |field, type_| error::Error::Field {
            field: field.to_string(),
            type_: type_.to_string(),
        })
        .await?;
        let arch = if let Some(arch) = ctx.args().get("arch") {
            Some(arch.clone())
        } else {
            node.get("arch").and_then(|x| x.as_string())
        };
        Ok(Self {
            addr: addr.clone(),
            arch,
            depends,
        })
    }
}

non_configurable!(ComposeTransform, error::Error);

#[async_trait]
impl TransformImpl for ComposeTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        let addr = Addr::parse("//default")?;
        Ok(addr)
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let mut hash = blake3::Hasher::new();
        let mut depend = self.depends.clone();
        depend.sort();
        for depend in depend.iter() {
            let t = ctx.get(depend).context(error::NotFoundSnafu {
                addr: depend.clone(),
            })?;
            let id = t.get_unique_id(ctx).await?;
            hash.update(id.digest().as_bytes());
        }
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        let arch = self
            .arch
            .as_ref()
            .map(|arch| ctx.args().get("arch").cloned().unwrap_or(arch.clone()));

        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest)
            .maybe_arch(arch)
            .build();
        trace!(component = "transform", type = "compose", "id is calculated to be {id}");
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self.depends.clone())
    }

    async fn prepare(&self, _log: &Log, _ctx: &Handle) -> TransformResult<()> {
        // Do nothing for a compose
        Ok(())
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        env.create_dir(Path::new("install-root")).await?;

        // Stage all the dependencies
        for dep in self.depends().await? {
            let t = ctx
                .get(&dep)
                .context(error::NotFoundSnafu { addr: dep.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            // TODO: We need to find a more portable way to do this than just assuming archives
            trace!(component = "transform", type = "compose", "staging dependencies {dep} with id {id} into install-root");
            let artifact = ctx.storage().safe_open(&id).await?;
            for layer in artifact.layers() {
                let reader = ctx.storage().safe_read(layer).await?;
                match layer.media_type() {
                    MediaType::Tar(..) => {
                        // let reader: Box<dyn AsyncRead> = match compression {
                        //     Compression::Bzip2 => Box::new(BzDecoder::new(BufReader::new(reader))),
                        //     Compression::Gzip => Box::new(GzipDecoder::new(BufReader::new(reader))),
                        //     Compression::Lz => Box::new(LzmaDecoder::new(BufReader::new(reader))),
                        //     Compression::Xz => Box::new(XzDecoder::new(BufReader::new(reader))),
                        //     Compression::Zstd => Box::new(ZstdDecoder::new(BufReader::new(reader))),
                        //     _ => Box::new(BufReader::new(reader)),
                        // };
                        env.unpack_stream(Path::new("install-root"), reader).await?;
                    }
                    value => {
                        warn!(component = "transform", type = "compose", "skipping artifact layer we do not know how to stage: {value}");
                    }
                }
            }
        }
        Ok(())
    }

    async fn transform(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;

            // Create the artifact manifest
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            // A Compose transform combines physically all the child dependents,
            // we should add a Combine transform that just does a layer collection.
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(Path::new("install-root"), writer.clone())
                .await?;
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
            Ok(artifact) => TransformStatus::Success(artifact),
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
    use snafu::Snafu;

    use edo::{
        context::{Addr, ContextError},
        transform::TransformError,
    };

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("could not find dependent transform with address {addr}"))]
        NotFound { addr: Addr },
        #[snafu(display("compose transform requires a field '{field}' with type '{type_}"))]
        Field { field: String, type_: String },
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
