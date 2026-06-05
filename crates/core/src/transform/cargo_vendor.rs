//! `cargo-vendor` transform.
//!
//! Runs `cargo vendor` against one or more Rust source trees inside a build
//! environment and packages the resulting `vendor/` directory plus a generated
//! `.cargo/config.toml` into a single artifact. The artifact's layout is:
//!
//! ```text
//! install-root/
//!   .cargo/
//!     config.toml      # points cargo at the vendored sources
//!     vendor/          # populated by `cargo vendor --locked`
//! ```
//!
//! Registered under the kind `cargo-vendor` by
//! [`crate::register_core`](crate::register_core).

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use snafu::OptionExt;
use std::{collections::BTreeMap, path::Path};
use std::{collections::VecDeque, path::PathBuf};

/// User configurable options for the CargoVendorTransform
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct CargoVendorOptions {
    #[serde(rename = "cargo-tomls", default)]
    cargo_tomls: BTreeMap<String, Vec<PathBuf>>,
}

/// A transform that runs `cargo vendor` over one or more sources and packages
/// the result (vendored crates + generated `.cargo/config.toml`) as an artifact.
///
/// Each entry in [`sources`](Self::sources) is staged into its own directory
/// inside the build environment. By default each source's top-level
/// `Cargo.toml` is picked up automatically; additional manifests can be listed
/// per-source via [`cargo_tomls`](Self::cargo_tomls), in which case they are
/// passed to cargo as `--manifest-path` (first) and `--sync` (rest).
pub struct CargoVendorTransform {
    /// Address this transform was registered under (used for unique-id naming).
    pub addr: Addr,
    /// Address of the [`Environment`] in which `cargo vendor` is executed.
    /// Defaults to `//default` when not specified in the node.
    pub environment: Addr,
    /// Named sources to vendor.
    /// Each source is staged into a directory named after its unique id.
    pub sources: BTreeMap<Addr, Source>,
    /// Optional override of which `Cargo.toml` files to feed cargo, keyed by
    /// the source name. Paths are relative to the source's staged directory.
    /// When omitted for a source, the source's root `Cargo.toml` is used (if
    /// present).
    pub cargo_tomls: BTreeMap<String, Vec<PathBuf>>,
}

#[async_trait]
impl FromElement for CargoVendorTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut options: CargoVendorOptions = element.get()?;
        let environment = element
            .environment
            .clone()
            .unwrap_or(Addr::parse("//default")?);
        let mut cargo_tomls = BTreeMap::new();
        for (key, value) in options.cargo_tomls.iter_mut() {
            value.sort();
            cargo_tomls.insert(key.clone(), value.clone());
        }
        let mut resolved = BTreeMap::new();
        let sources = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .and_then(|x| x.get("default"))
            .context(error::NoSourcesSnafu {
                addr: element.addr.clone(),
            })?;
        for src in sources {
            resolved.insert(src.addr.clone(), ctx.add_source(src).await?);
        }

        Ok(Self {
            addr: element.addr.clone(),
            environment,
            sources: resolved,
            cargo_tomls,
        })
    }
}

#[async_trait]
impl TransformImpl for CargoVendorTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.environment.clone())
    }

    /// Computes a deterministic id from the environment address and the
    /// unique ids of every input source. Changing any source (or the
    /// environment) invalidates the cached output.
    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        let mut hash = blake3::Hasher::new();
        hash.update(self.environment.to_string().as_bytes());
        for source in self.sources.values() {
            let source_id = source.get_unique_id().await?;
            hash.update(source_id.digest().as_bytes());
        }

        // We need to hash the cargo_tomls as a change in this field
        // would require a rebuild
        for (key, value) in self.cargo_tomls.iter() {
            for entry in value {
                let unique = format!("{key}-{entry:?}");
                hash.update(unique.as_bytes());
            }
        }

        let digest = hash.finalize();
        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest.to_hex().to_lowercase())
            .build();
        trace!(subsystem = "transform", component = "cargo-vendor", id = %id, "calculated id");
        Ok(id.clone())
    }

    /// `cargo-vendor` does not depend on other transforms — its inputs are
    /// purely [`Source`]s, which are fetched in [`prepare`](Self::prepare).
    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::default())
    }

    /// Short-circuits prepare when every vendored source is already in
    /// the local cache.
    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        for source in self.sources.values() {
            if !source.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Caches every input source into the context's source storage so that
    /// [`stage`](Self::stage) can lay them out in the environment without
    /// further network access.
    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // Fetch the source we are vendoring code for
        for source in self.sources.values() {
            source.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    /// Stages each source into a directory named after its unique id inside
    /// the build environment. The id-named layout keeps multiple sources
    /// from colliding and makes the staged paths content-addressed.
    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // For each source we are going to stage things into addr centered directories
        for (_, source) in self.sources.iter() {
            let id = source.get_unique_id().await?;
            trace!(
                subsystem = "transform",
                component = "cargo-vendor",
                op = "stage",
                id = %id,
                "staging source"
            );
            let string = id.to_string();
            let dir = Path::new(&string);
            env.create_dir(dir).await?;
            env.stage(
                ctx,
                ArtifactStageOptions::builder().id(id).path(dir).build(),
            )
            .await?;
        }
        Ok(())
    }

    /// Builds the vendored artifact:
    ///
    /// 1. Creates `install-root/.cargo/vendor/` in the environment's VFS.
    /// 2. Collects the `Cargo.toml` paths to pass to cargo (overrides from
    ///    [`cargo_tomls`](Self::cargo_tomls), then any auto-detected root
    ///    manifests).
    /// 3. Runs `cargo vendor --locked --manifest-path <first> [--sync <rest>]
    ///    .cargo/vendor`.
    /// 4. Writes a `.cargo/config.toml` that redirects `crates-io` to the
    ///    vendored directory.
    /// 5. Tars `install-root/` into a single uncompressed layer and saves it
    ///    as a [`MediaType::Manifest`] artifact.
    ///
    /// Errors are surfaced as [`TransformStatus::Retryable`] so the scheduler
    /// may attempt the transform again.
    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            let install_root = vfs.create_dir("install-root").await?;
            let cargo_dir = install_root.create_dir(".cargo").await?;
            let vendor_dir = cargo_dir.create_dir("vendor").await?;

            // Now we want to gather our directory paths
            let mut cargo_tomls = VecDeque::new();
            for (addr, source) in self.sources.iter() {
                let src_id = source.get_unique_id().await?;
                let src_dir = vfs.entry(src_id.to_string()).await;
                let name = addr.last().unwrap();
                // If we have an override use those paths for the cargo tomls and assume their presence
                if let Some(list) = self.cargo_tomls.get(name) {
                    for item in list {
                        let toml = src_dir.entry(item).await;
                        cargo_tomls.push_back(toml);
                    }
                }
                if src_dir.try_exists("Cargo.toml").await? {
                    cargo_tomls.push_back(src_dir.entry("Cargo.toml").await);
                }
            }
            // We now want to gather the cargo_tomls into arguments where the first is --manifest-path
            // and following are --sync
            let mut args: Vec<&str> = Vec::new();
            args.push("vendor");
            args.push("--locked");
            args.push("--manifest-path");
            let first_manifest = cargo_tomls.pop_front().context(error::NoCargoSnafu)?;
            args.push(first_manifest.as_ref());
            for toml in cargo_tomls.iter() {
                args.push("--sync");
                args.push(toml.as_ref());
            }
            args.push(vendor_dir.as_ref());

            // Now we want to execute the command
            vfs.command("cargo-vendor", "cargo", args).await?;

            // Now we want to generate the config.toml
            let cargo_toml = format!(
                r###"[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = ".cargo/vendor"
"###
            );
            cargo_dir
                .write("config.toml", cargo_toml.as_bytes())
                .await?;

            // Now we build an artifact containing an archive of the resulting vendoring
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id).build())
                .media_type(MediaType::Manifest)
                .build();

            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(install_root.path(), writer.clone()).await?;

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

            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e),
        }
    }

    /// `cargo-vendor` supports interactive shelling for debugging vendoring
    /// failures inside the build environment.
    fn can_shell(&self) -> bool {
        true
    }

    /// Drops the user into a shell at the environment root so they can
    /// inspect staged sources or rerun `cargo vendor` manually.
    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("."))?;
        Ok(())
    }
}

/// Errors produced while loading or running a [`CargoVendorTransform`].
pub mod error {
    use snafu::Snafu;

    use edo::{
        context::{Addr, ContextError},
        source::SourceError,
        transform::TransformError,
    };

    /// Errors raised by the `cargo-vendor` transform.
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
        #[snafu(display("invalid cargo vendor transform at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        /// Error if there are no cargo.tomls found
        #[snafu(display("no Cargo.toml files were found to vendor"))]
        NoCargo,
        /// No sources were provided
        #[snafu(display("no sources were provided to cargo vendor transform at {addr}"))]
        NoSources { addr: Addr },
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
