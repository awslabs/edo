#[macro_use]
extern crate tracing;

use edo::{
    context::{Context, Definable, DefinableNoContext},
    environment::Farm,
    source::{Source, Vendor},
    storage::Backend,
    transform::Transform,
};
use environment::{ContainerFarm, LocalFarm};
use source::{GitSource, ImageSource, LocalSource, RemoteSource, VendorSource};
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
        Arc::new(async |addr, node, ctx: Context| {
            Ok(Backend::new(
                S3Backend::new(&addr, &node, ctx.config()).await?,
            ))
        }),
    );
    registry.register_farm(
        "local",
        Arc::new(async |addr, node, ctx| Ok(Farm::new(LocalFarm::new(&addr, &node, &ctx).await?))),
    );
    registry.register_farm(
        "container",
        Arc::new(async |addr, node, ctx| {
            Ok(Farm::new(ContainerFarm::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_source(
        "git",
        Arc::new(async |addr, node, ctx| {
            Ok(Source::new(GitSource::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_source(
        "local",
        Arc::new(async |addr, node, ctx| {
            Ok(Source::new(LocalSource::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_source(
        "image",
        Arc::new(async |addr, node, ctx| {
            Ok(Source::new(ImageSource::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_source(
        "remote",
        Arc::new(async |addr, node, ctx| {
            Ok(Source::new(RemoteSource::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_source(
        "vendor",
        Arc::new(async |addr, node, ctx| {
            Ok(Source::new(VendorSource::new(&addr, &node, &ctx).await?))
        }),
    );
    registry.register_transform(
        "compose",
        Arc::new(async |addr, node, ctx| {
            Ok(Transform::new(
                ComposeTransform::new(&addr, &node, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "import",
        Arc::new(async |addr, node, ctx| {
            Ok(Transform::new(
                ImportTransform::new(&addr, &node, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "script",
        Arc::new(async |addr, node, ctx| {
            Ok(Transform::new(
                ScriptTransform::new(&addr, &node, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "cargo-vendor",
        Arc::new(async |addr, node, ctx| {
            Ok(Transform::new(
                CargoVendorTransform::new(&addr, &node, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "go-vendor",
        Arc::new(async |addr, node, ctx| {
            Ok(Transform::new(
                GoVendorTransform::new(&addr, &node, &ctx).await?,
            ))
        }),
    );
    registry.register_vendor(
        "image",
        Arc::new(async |addr, node, ctx| {
            Ok(Vendor::new(ImageVendor::new(&addr, &node, &ctx).await?))
        }),
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
