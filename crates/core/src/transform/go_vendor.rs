//! `go-vendor` transform.
//!
//! Runs `go mod vendor` against one or more Go modules inside a build
//! environment and packages the resulting `vendor/` directories into an
//! artifact that overlays cleanly on top of the original source tree.
//!
//! Unlike [`cargo_vendor`](super::cargo_vendor), this transform takes a
//! **single** [`Source`] because Go vendoring is module-path sensitive: the
//! `vendor/` directory must sit next to the `go.mod` it belongs to. Multiple
//! modules within that one source tree can still be vendored by listing them
//! in [`GoVendorTransform::modules`].
//!
//! Produced artifact layout:
//!
//! ```text
//! install-root/
//!   <module-path>/vendor/   # one per entry in `modules`, or just `vendor/`
//!                           # at the root when `modules` is empty
//! ```
//!
//! Registered under the kind `go-vendor` by
//! [`crate::register_core`](crate::register_core).

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, FromNode, Handle, Log, Node},
    environment::{Environment, Vfs},
    non_configurable,
    source::Source,
    storage::{Artifact, Compression, Config, Id, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use snafu::OptionExt;
use std::path::Path;

/// A transform that runs `go mod vendor` over one or more modules of a single
/// source and packages the resulting `vendor/` directories as an artifact.
///
/// The output is structured so that overlaying it onto the original source
/// tree drops each `vendor/` directory next to its corresponding `go.mod`.
pub struct GoVendorTransform {
    /// Address this transform was registered under (used for unique-id naming).
    pub addr: Addr,
    /// Address of the [`Environment`] in which `go mod vendor` is executed.
    /// Defaults to `//default` when not specified in the node.
    pub environment: Addr,
    /// The single source containing the Go module(s) to vendor. Go vendoring
    /// is path-sensitive, so only one source is supported — use
    /// [`modules`](Self::modules) to vendor multiple modules within it.
    pub source: Source,
    /// Module sub-paths within [`source`](Self::source) to vendor, relative
    /// to the source root. Empty means "vendor the root module only".
    pub modules: Vec<String>,
}

#[async_trait]
impl FromNode for GoVendorTransform {
    type Error = error::Error;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self, error::Error> {
        let environment = if let Some(n) = node.get("environment") {
            Addr::parse(&n.as_string().context(error::FieldSnafu {
                field: "environment",
                type_: "string",
            })?)?
        } else {
            Addr::parse("//default")?
        };
        // Go Vendor only supports a single source due to go vendoring being module path specific
        let src = node.get("source").context(error::FieldSnafu {
            field: "source",
            type_: "node",
        })?;
        let source = ctx.add_source(addr, &src).await?;

        // We do though support multiple paths, if this list is empty we assume just root path module
        let modules = if let Some(list) = node.get("modules").and_then(|x| x.as_list()) {
            let mut array = Vec::new();
            for item in list.iter() {
                array.push(item.as_string().context(error::FieldSnafu {
                    field: "modules",
                    type_: "list[string]",
                })?);
            }
            array
        } else {
            Vec::new()
        };
        Ok(Self {
            addr: addr.clone(),
            environment,
            source,
            modules,
        })
    }
}

non_configurable!(GoVendorTransform, error::Error);

#[async_trait]
impl TransformImpl for GoVendorTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.environment.clone())
    }

    /// Computes a deterministic id from the environment address and the
    /// source's unique id. Changing either invalidates the cached output.
    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        let mut hash = blake3::Hasher::new();
        hash.update(self.environment.to_string().as_bytes());
        let source_id = self.source.get_unique_id().await?;
        hash.update(source_id.digest().as_bytes());
        for module in self.modules.iter() {
            hash.update(module.as_bytes());
        }
        let digest = hash.finalize();
        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest.to_hex().to_lowercase())
            .build();
        trace!(component = "transform", type = "go-vendor", "id is calculated to be {id}");
        Ok(id.clone())
    }

    /// `go-vendor` does not depend on other transforms — its only input is
    /// the configured [`Source`], which is fetched in
    /// [`prepare`](Self::prepare).
    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::default())
    }

    /// Caches the input source into the context's source storage so that
    /// [`stage`](Self::stage) can lay it out in the environment without
    /// further network access.
    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // Fetch the source we are vendoring code for
        self.source.cache(log, ctx.storage()).await?;
        Ok(())
    }

    /// Stages the source flat under `build-root/` in the environment. Go
    /// vendoring is path-centric, so module paths from
    /// [`modules`](Self::modules) are interpreted relative to this directory.
    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Due to go vendoring being very path centric we expect that if we are given multiple sources
        // they will be used staged flattened out
        let build_root = Path::new("build-root");
        env.create_dir(build_root).await?;

        // For each source we are going to stage things into addr centered directories
        self.source
            .stage(log, ctx.storage(), env, build_root)
            .await?;
        Ok(())
    }

    /// Builds the vendored artifact:
    ///
    /// 1. For each module path (or just the source root if
    ///    [`modules`](Self::modules) is empty), checks for a `go.mod`.
    /// 2. Runs `go mod vendor` from inside that module's directory.
    /// 3. Copies the resulting `vendor/` directory into
    ///    `install-root/<module-path>/vendor/`.
    /// 4. Tars `install-root/` into a single uncompressed layer and saves it
    ///    as a [`MediaType::Manifest`] artifact.
    ///
    /// Errors are surfaced as [`TransformStatus::Retryable`] so the scheduler
    /// may attempt the transform again.
    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            let build_root = vfs.entry("build-root").await;
            let install_root = vfs.create_dir("install-root").await?;

            // If we don't have modules list specified we assume root only
            let paths = if self.modules.is_empty() {
                vec![(build_root, install_root.clone())]
            } else {
                let mut paths = Vec::new();
                for path in self.modules.iter() {
                    paths.push((build_root.entry(path).await, install_root.create_dir(path).await?));
                }
                paths
            };


            for (src_path, target_path) in paths.iter() {
                if src_path.try_exists("go.mod").await? {
                    // Found a go module to vendor
                    trace!(component = "transform", type = "go-vendor", "vendoring go sources for module at {:?}", src_path.path());
                    src_path.command("go-vendor", "go", &["mod", "vendor"]).await?;
                    // Copy the resulting vendor directory into target_path
                    let target_vendor = target_path.entry("vendor").await;
                    let src_vendor = src_path.entry("vendor").await;
                    vfs.copy(src_vendor, target_vendor).await?;
                }
            }
            // Now we build an artifact containing an archive of the resulting vendor directories
            // that can overlay with the source
            let writer = ctx.storage().safe_start_layer().await?;
            env.read(install_root.path(), writer.clone()).await?;
            let layer = ctx.storage().safe_finish_layer(&MediaType::Tar(Compression::None), None, &writer).await?;

            let artifact = Artifact::builder()
                .config(Config::builder()
                    .id(id)
                    .build()
                )
                .media_type(MediaType::Manifest)
                .layers(vec![layer])
                .build();

            ctx.storage().safe_save(&artifact).await?;

            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e),
        }
    }

    /// `go-vendor` supports interactive shelling for debugging vendoring
    /// failures inside the build environment.
    fn can_shell(&self) -> bool {
        true
    }

    /// Drops the user into a shell at the environment root so they can
    /// inspect the staged source or rerun `go mod vendor` manually.
    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("."))?;
        Ok(())
    }
}

/// Errors produced while loading or running a [`GoVendorTransform`].
pub mod error {
    use snafu::Snafu;

    use edo::{context::ContextError, source::SourceError, transform::TransformError};

    /// Errors raised by the `go-vendor` transform.
    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        /// Bubbled up from the engine [`Context`](edo::context::Context) layer
        /// (e.g. parsing an [`Addr`](edo::context::Addr) failed).
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        /// A required TOML field was missing or had the wrong shape.
        #[snafu(display(
            "go-vendor transform definitions require a field '{field}' with type_ '{type_}'"
        ))]
        Field {
            /// Name of the missing or ill-typed field.
            field: String,
            /// Human-readable description of the expected type.
            type_: String,
        },
        /// Bubbled up from the [`Source`](edo::source::Source) subsystem
        /// (fetch / stage / id computation).
        #[snafu(transparent)]
        Source {
            #[snafu(source(from(SourceError, Box::new)))]
            source: Box<SourceError>,
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
