use std::collections::HashMap;
use std::path::{Path, PathBuf};

use edo::context::{Addr, Context, Element, FromElement, Handle, Log};
use edo::environment::{Environment, Vfs};
use edo::source::Source;
use edo::storage::{
    Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType,
};
use edo::transform::{TransformError, TransformImpl, TransformResult, TransformStatus};

use async_trait::async_trait;
use handlebars::Handlebars;
use indexmap::IndexMap;
use ocilot::models::Platform;
use snafu::{OptionExt, ResultExt};

#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScriptOptions {
    arch: Option<String>,
    #[serde(default)]
    depends: Vec<Addr>,
    commands: Vec<String>,
    #[serde(default = "default_interpreter")]
    interpreter: String,
    artifact: Option<PathBuf>,
}

fn default_interpreter() -> String {
    "bash".to_string()
}

/// A transform that executes shell commands in a build environment to produce an artifact.
pub struct ScriptTransform {
    pub addr: Addr,
    pub environment: Addr,
    pub sources: IndexMap<String, Vec<Source>>,
    pub options: ScriptOptions,
}

#[async_trait]
impl FromElement for ScriptTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut options: ScriptOptions = element.get()?;
        options.arch = if options.arch.is_none()
            && let Some(arch) = ctx.args().get("arch")
        {
            Some(arch.clone())
        } else {
            options.arch
        };
        let environment = element
            .environment
            .clone()
            .unwrap_or(Addr::parse("//default").unwrap());
        let mut sources = IndexMap::new();
        for (scope, source_list) in element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default()
        {
            let mut entries = Vec::new();
            for source in source_list.iter() {
                entries.push(ctx.add_source(source).await?);
            }
            sources.insert(scope.clone(), entries);
        }
        Ok(Self {
            addr: element.addr.clone(),
            environment,
            sources,
            options,
        })
    }
}

#[async_trait]
impl TransformImpl for ScriptTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.environment.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        // Digest will be a merkle hash of:
        // all sources digest + script contents
        let mut hash = blake3::Hasher::new();
        let mut depends = self.options.depends.clone();
        depends.sort();
        for depend in depends.iter() {
            // We should use the resolved id for the dependency
            let t = ctx.get(depend).context(error::NotFoundSnafu {
                addr: depend.clone(),
            })?;
            let id = t.get_unique_id(ctx).await?;
            hash.update(id.digest().as_bytes());
        }
        for source_list in self.sources.values() {
            for source in source_list {
                let source_id = source.get_unique_id().await?;
                hash.update(source_id.digest().as_bytes());
            }
        }
        let script = self.options.commands.join("\n");
        hash.update(script.as_bytes());
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_bytes());
        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest)
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(component = "transform", type = "script", "id is calculated to be {id}");
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self.options.depends.clone())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // We should fetch all our sources
        for source_list in self.sources.values() {
            for source in source_list {
                source.cache(log, ctx.storage()).await?;
            }
        }
        Ok(())
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
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
            // Use Environment::stage so decompression, archive vs file
            // dispatch, and path_hint placement are handled uniformly
            // (matching how sources are staged below).
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(build_root)
                    .build(),
            )
            .await?;
        }

        // Stage all sources in our build-root
        for source_list in self.sources.values() {
            for source in source_list {
                let id = source.get_unique_id().await?;
                trace!(component = "transform", type = "script", "staging source {id}");
                env.stage(
                    ctx,
                    ArtifactStageOptions::builder()
                        .id(id)
                        .path(build_root)
                        .build(),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            // Run the script in our environment
            let id = self.get_unique_id(ctx).await?;
            // Handlebars defaults to HTML-escaping interpolated values
            // (`&` → `&amp;`, `<` → `&lt;`, etc.) which corrupts shell
            // commands — e.g. `--arg url='https://h?a=1&b=2'` would render
            // with the ampersand escaped into the generated script. The
            // script template is producing shell, not HTML, so swap in
            // `no_escape` to render values verbatim.
            let mut handlebars = Handlebars::new();
            handlebars.register_escape_fn(handlebars::no_escape);
            let vfs = Vfs::new(&id, env, log).await?;

            let mut script = vec![format!("#!/usr/bin/env {}", self.options.interpreter)];
            let build_root = vfs.create_dir("build-root").await?;
            let install_root = vfs.create_dir("install-root").await?;
            let mut commands = self.options.commands.clone();

            script.append(&mut commands);

            let arch = if let Some(arch) = self.options.arch.as_ref() {
                arch.as_str()
            } else {
                std::env::consts::ARCH
            };
            let mut args = HashMap::from([
                ("build-root", build_root.path().to_str().unwrap()),
                ("install-root", install_root.path().to_str().unwrap()),
                ("arch", arch),
            ]);

            for (key, value) in ctx.args() {
                if key == "arch" {
                    continue;
                }
                args.insert(key, value);
            }

            let script_string = script.join("\n");
            let resolved_script = handlebars
                .render_template(&script_string, &args)
                .context(error::RenderSnafu)?;

            // Write the script into the vfs
            let script = vfs.entry("script.sh").await;
            vfs.write(script.path(), resolved_script.as_bytes()).await?;
            // Make it executable
            vfs.command("chmod", "chmod", &["+x", AsRef::<str>::as_ref(&script)])
                .await?;
            // Run the script via the configured interpreter so the script
            // path resolves regardless of whether `.` is in PATH. Dispatch
            // through the build-root vfs so the script's working directory is
            // build-root, matching the pre-refactor behaviour where scripts
            // could reference staged sources by build-root-relative paths.
            build_root
                .command(
                    "script",
                    &self.options.interpreter,
                    &[AsRef::<str>::as_ref(&script)],
                )
                .await?;

            // The result of a script transform is everything put in the install-root
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            // Open a layer to store the result in
            let writer = ctx.storage().safe_start_layer().await?;
            let mut apath = PathBuf::from("install-root");
            if let Some(path) = self.options.artifact.as_ref() {
                apath = apath.join(path);
            }
            env.read_stream(apath.as_path(), writer.clone()).await?;
            artifact.layers_mut().push(
                ctx.storage()
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder()
                            .media_type(MediaType::Tar(Compression::None))
                            .platform(
                                Platform::builder()
                                    .os(std::env::consts::OS)
                                    .architecture(
                                        self.options
                                            .arch
                                            .clone()
                                            .unwrap_or(std::env::consts::ARCH.to_string()),
                                    )
                                    .build(),
                            )
                            .build(),
                    )
                    .await?,
            );
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact),
            // We always assume a script transform is retryable. Move the
            // owned `TransformError` into the status so the snafu source
            // chain is preserved (don't stringify via `e.to_string()`).
            Err(e) => TransformStatus::Retryable(Some(log.path()), e),
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
        #[snafu(display("invalid script transform definition at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("could not find dependent transform with address {addr}"))]
        NotFound { addr: Addr },
        #[snafu(display("failed to render handlebars template for script: {source}"))]
        Render { source: handlebars::RenderError },
        #[snafu(display("script path is not valid utf-8"))]
        ScriptPath,
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
