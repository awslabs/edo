#[macro_use]
extern crate tracing;

use edo::{
    context::{Context, FromElement, FromElementNoContext},
    environment::Farm,
    source::{Source, Vendor},
    storage::Backend,
    transform::Transform,
};
use environment::{ContainerFarm, LocalFarm};
use source::{GitSource, ImageSource, LocalSource, RemoteSource};
use std::sync::Arc;
use storage::S3Backend;
use transform::{ComposeTransform, ImportTransform, ScriptTransform};
use vendor::ImageVendor;

use crate::transform::{CargoVendorTransform, GoVendorTransform};
/// Environments and Farms
pub mod environment;
/// Sources
pub mod source;
/// Storage backends
pub mod storage;
/// Transforms
pub mod transform;
/// Vendors
pub mod vendor;

/// Registers all built-in component implementations (sources, transforms, environments, storage backends, vendors) with the given context.
pub fn register_core(ctx: &Context) {
    let registry = ctx.registry();
    registry.register_backend(
        "s3",
        Arc::new(async |element, ctx: Context| {
            Ok(Backend::new(S3Backend::new(&element, ctx.config()).await?))
        }),
    );
    registry.register_farm(
        "local",
        Arc::new(async |element, ctx| Ok(Farm::new(LocalFarm::new(&element, &ctx).await?))),
    );
    registry.register_farm(
        "container",
        Arc::new(async |element, ctx| Ok(Farm::new(ContainerFarm::new(&element, &ctx).await?))),
    );
    registry.register_source(
        "git",
        Arc::new(async |element, ctx| Ok(Source::new(GitSource::new(&element, &ctx).await?))),
    );
    registry.register_source(
        "local",
        Arc::new(async |element, ctx| Ok(Source::new(LocalSource::new(&element, &ctx).await?))),
    );
    registry.register_source(
        "image",
        Arc::new(async |element, ctx| Ok(Source::new(ImageSource::new(&element, &ctx).await?))),
    );
    registry.register_source(
        "remote",
        Arc::new(async |element, ctx| Ok(Source::new(RemoteSource::new(&element, &ctx).await?))),
    );
    registry.register_transform(
        "compose",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(ComposeTransform::new(&element, &ctx).await?))
        }),
    );
    registry.register_transform(
        "import",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(ImportTransform::new(&element, &ctx).await?))
        }),
    );
    registry.register_transform(
        "script",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(ScriptTransform::new(&element, &ctx).await?))
        }),
    );
    registry.register_transform(
        "cargo-vendor",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                CargoVendorTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "go-vendor",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                GoVendorTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_vendor(
        "image",
        Arc::new(async |element, ctx| Ok(Vendor::new(ImageVendor::new(&element, &ctx).await?))),
    );
}
/// Error types for the core plugin.
pub mod error {
    use edo::context::ContextError;
    use snafu::Snafu;

    /// Errors produced when registering or resolving core plugin components.
    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        ContainerEnv {
            #[snafu(source(from(crate::environment::container::error::Error, Box::new)))]
            source: Box<crate::environment::container::error::Error>,
        },
        #[snafu(transparent)]
        Git {
            #[snafu(source(from(crate::source::git::error::Error, Box::new)))]
            source: Box<crate::source::git::error::Error>,
        },
        #[snafu(transparent)]
        LocalEnv {
            #[snafu(source(from(crate::environment::local::error::Error, Box::new)))]
            source: Box<crate::environment::local::error::Error>,
        },
        #[snafu(transparent)]
        LocalSource {
            #[snafu(source(from(crate::source::local::error::Error, Box::new)))]
            source: Box<crate::source::local::error::Error>,
        },
        #[snafu(display("no implementation for a storage backend with kind '{kind}'"))]
        NoBackend { kind: String },
        #[snafu(display("only definitions with a kind can be parsed"))]
        NoKind,
        #[snafu(display("no implementation for an environment farm with kind '{kind}"))]
        NoFarm { kind: String },
        #[snafu(display("no implementation for a source with kind '{kind}'"))]
        NoSource { kind: String },
        #[snafu(display("no implementation for a transform with kind '{kind}'"))]
        NoTransform { kind: String },
        #[snafu(display("no implementation for a vendor with kind '{kind}"))]
        NoVendor { kind: String },
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
