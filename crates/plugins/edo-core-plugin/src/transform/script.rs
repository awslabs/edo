use std::path::{Path, PathBuf};

use edo_core::context::{Addr, Context, FromNode, Handle, Log, Node, non_configurable};
use edo_core::environment::Environment;
use edo_core::source::Source;
use edo_core::storage::{
    Artifact, ArtifactBuilder, Compression, ConfigBuilder, Id, IdBuilder, MediaType,
};
use edo_core::transform::{TransformError, TransformImpl, TransformResult, TransformStatus};

use async_trait::async_trait;
use indexmap::IndexMap;
use ocilot::models::PlatformBuilder;
use snafu::OptionExt;

pub struct ScriptTransform {
    pub addr: Addr,
    pub arch: Option<String>,
    pub environment: Addr,
    pub depends: Vec<Addr>,
    pub commands: Vec<String>,
    pub interpreter: String,
    pub artifact: Option<PathBuf>,
    pub sources: IndexMap<String, Source>,
}

#[async_trait]
impl FromNode for ScriptTransform {
    type Error = error::Error;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self, error::Error> {
        node.validate_keys(&["commands"])?;
        let environment = if let Some(n) = node.get("environment") {
            Addr::parse(&n.as_string().context(error::FieldSnafu {
                field: "environment",
                type_: "string",
            })?)?
        } else {
            Addr::parse("//default")?
        };
        let interpreter = if let Some(n) = node.get("interpreter") {
            n.as_string()
                .context(error::FieldSnafu {
                    field: "interpreter",
                    type_: "string",
                })?
                .clone()
        } else {
            "bash".to_string()
        };
        let mut commands: Vec<String> = Vec::new();
        for line in node
            .get("commands")
            .unwrap()
            .as_list()
            .context(error::FieldSnafu {
                field: "commands",
                type_: "list of strings",
            })?
        {
            commands.push(
                line.as_string()
                    .context(error::FieldSnafu {
                        field: "commands",
                        type_: "list of strings",
                    })?
                    .clone(),
            );
        }
        let artifact = if let Some(n) = node.get("artifact") {
            Some(PathBuf::from(n.as_string().context(error::FieldSnafu {
                field: "artifact",
                type_: "string",
            })?))
        } else {
            None
        };
        let field_error = |field: &str, type_: &str| error::Error::Field {
            field: field.to_string(),
            type_: type_.to_string(),
        };
        let depends = super::parse_depends(node, "depends", field_error).await?;
        let sources = super::parse_sources(addr, node, ctx, field_error).await?;
        Ok(Self {
            addr: addr.clone(),
            arch: if let Some(arch) = ctx.args().get("arch") {
                Some(arch.clone())
            } else {
                node.get("arch").and_then(|x| x.as_string())
            },
            environment,
            depends,
            interpreter,
            commands,
            sources,
            artifact,
        })
    }
}

non_configurable!(ScriptTransform, error::Error);

#[async_trait]
impl TransformImpl for ScriptTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.environment.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        // Digest will be a merkle hash of:
        // all sources digest + script contents
        let mut hash = blake3::Hasher::new();
        let mut depends = self.depends.clone();
        depends.sort();
        for depend in depends.iter() {
            // We should use the resolved id for the dependency
            let t = ctx.get(depend).context(error::NotFoundSnafu {
                addr: depend.clone(),
            })?;
            let id = t.get_unique_id(ctx).await?;
            hash.update(id.digest().as_bytes());
        }
        for source in self.sources.values() {
            let source_id = source.get_unique_id().await?;
            hash.update(source_id.digest().as_bytes());
        }
        let script = self.commands.join("\n");
        hash.update(script.as_bytes());
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        let arch = self
            .arch
            .as_ref()
            .map(|arch| ctx.args().get("arch").cloned().unwrap_or(arch.clone()));
        let id = IdBuilder::default()
            .name(self.addr.to_id())
            .digest(digest)
            .version(None)
            .arch(arch)
            .build()
            .unwrap();
        trace!(component = "transform", type = "script", "id is calculated to be {id}");
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self.depends.clone())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // We should fetch all our sources
        for source in self.sources.values() {
            source.fetch(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // First we want to create our build-root and install-roots
        let build_root = Path::new("build-root");
        env.create_dir(build_root).await?;
        env.create_dir(Path::new("install-root")).await?;

        // Stage all dependencies into the build-root
        for dep in self.depends().await? {
            let t = ctx
                .get(&dep)
                .context(error::NotFoundSnafu { addr: dep.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(component = "transform", type = "script", "staging dependency {dep} with id {id}");
            let artifact = ctx.storage().safe_open(&id).await?;
            for layer in artifact.layers() {
                let reader = ctx.storage().safe_read(layer).await?;
                match layer.media_type() {
                    MediaType::Tar(..) => {
                        env.unpack(build_root, reader).await?;
                    }
                    _ => {
                        warn!(
                            "skipping stage for dependency layer that we do not know how to stage"
                        );
                    }
                }
            }
        }

        // Stage all sources in our build-root
        for (addr, source) in self.sources.iter() {
            trace!(component = "transform", type = "script", "staging source {addr}");
            source.stage(log, ctx.storage(), env, build_root).await?;
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            // Run the script in our environment
            let id = self.get_unique_id(ctx).await?;
            let mut cmd = env.defer_cmd(log, &id);
            cmd.set_interpreter(self.interpreter.as_str());
            cmd.create_named_dir("build-root", "build-root").await?;
            cmd.create_named_dir("install-root", "install-root").await?;
            if let Some(arch) = self.arch.as_ref() {
                cmd.set("arch", arch.as_str())?;
            } else {
                cmd.set("arch", std::env::consts::ARCH)?;
            }
            for (key, value) in ctx.args() {
                if key == "arch" {
                    continue;
                }
                cmd.set(key, value)?;
            }

            for command in self.commands.iter() {
                cmd.run(command).await?;
            }

            cmd.send("{{build-root}}").await?;

            // The result of a script transform is everything put in the install-root
            let mut artifact = ArtifactBuilder::default()
                .config(ConfigBuilder::default().id(id.clone()).build().unwrap())
                .media_type(MediaType::Manifest)
                .build()
                .unwrap();

            // Open a layer to store the result in
            let writer = ctx.storage().safe_start_layer().await?;
            let mut apath = PathBuf::from("install-root");
            if let Some(path) = self.artifact.as_ref() {
                apath = apath.join(path);
            }
            env.read(apath.as_path(), writer.clone()).await?;
            artifact.layers_mut().push(
                ctx.storage()
                    .safe_finish_layer(
                        &MediaType::Tar(Compression::None),
                        Some(
                            PlatformBuilder::default()
                                .os(std::env::consts::OS)
                                .architecture(
                                    self.arch
                                        .clone()
                                        .unwrap_or(std::env::consts::OS.to_string()),
                                )
                                .build()
                                .unwrap(),
                        ),
                        &writer,
                    )
                    .await?,
            );
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        .as_ref()
        {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            // We always assume a script transform is retryable
            Err(e) => TransformStatus::Retryable(
                Some(log.path()),
                error::Error::Failed {
                    message: e.to_string(),
                }
                .into(),
            ),
        }
    }

    fn can_shell(&self) -> bool {
        true
    }

    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("build-root"))?;
        Ok(())
    }
}

pub mod error {
    use snafu::Snafu;

    use edo_core::{
        context::{Addr, ContextError},
        plugin::error::PluginError,
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
        #[snafu(display("{message}"))]
        Failed { message: String },
        #[snafu(display(
            "script transform definitions require a field '{field}' with type_ '{type_}'"
        ))]
        Field { field: String, type_: String },
        #[snafu(display("could not find dependent transform with address {addr}"))]
        NotFound { addr: Addr },
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
