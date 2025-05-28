use crate::{
    context::{Addr, Context, Definable, DefinableNoContext, Log, Node},
    environment::Farm,
    plugin::{Plugin, PluginImpl, Result as PluginResult},
    source::Source,
    source::Vendor,
    storage::{Backend, Storage},
    transform::Transform,
};
use async_trait::async_trait;
use environment::{ContainerFarm, LocalFarm};
use snafu::OptionExt;
use source::{GitSource, ImageSource, LocalSource, RemoteSource, VendorSource};
use storage::{LocalBackend, S3Backend};
use transform::{ComposeTransform, ImportTransform, ScriptTransform};
use vendor::ImageVendor;
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

pub fn core_plugin() -> Plugin {
    Plugin::from_impl(CorePlugin)
}

/// This acts as our inline plugin for all builtin constructs
#[derive(Default)]
pub struct CorePlugin;

#[async_trait]
impl PluginImpl for CorePlugin {
    async fn fetch(&self, _log: &Log, _storage: &Storage) -> PluginResult<()> {
        Ok(())
    }

    async fn setup(&self, _log: &Log, _storage: &Storage) -> PluginResult<()> {
        Ok(())
    }

    async fn create_storage(
        &self,
        addr: &Addr,
        node: &Node,
        ctx: &Context,
    ) -> PluginResult<Backend> {
        let kind = node.get_kind().context(error::NoKindSnafu)?;
        match kind.as_str() {
            "local" => Ok(Backend::from_impl(
                LocalBackend::new(addr, node, ctx.config()).await?,
            )),
            "s3" => Ok(Backend::from_impl(
                S3Backend::new(addr, node, ctx.config()).await?,
            )),
            _ => error::NoBackendSnafu { kind }.fail().map_err(|e| e.into()),
        }
    }

    async fn create_farm(&self, addr: &Addr, node: &Node, ctx: &Context) -> PluginResult<Farm> {
        let kind = node.get_kind().context(error::NoKindSnafu)?;
        match kind.as_str() {
            "local" => Ok(Farm::from_impl(LocalFarm::new(addr, node, ctx).await?)),
            "container" => Ok(Farm::from_impl(ContainerFarm::new(addr, node, ctx).await?)),
            _ => error::NoFarmSnafu { kind }.fail().map_err(|e| e.into()),
        }
    }

    async fn create_source(&self, addr: &Addr, node: &Node, ctx: &Context) -> PluginResult<Source> {
        let kind = node.get_kind().context(error::NoKindSnafu)?;
        debug!(
            section = "core-plugin",
            component = "source",
            "create source {addr} with kind {kind}"
        );
        match kind.as_str() {
            "git" => Ok(Source::from_impl(GitSource::new(addr, node, ctx).await?)),
            "local" => Ok(Source::from_impl(LocalSource::new(addr, node, ctx).await?)),
            "image" => Ok(Source::from_impl(ImageSource::new(addr, node, ctx).await?)),
            "remote" => Ok(Source::from_impl(RemoteSource::new(addr, node, ctx).await?)),
            "vendor" => Ok(Source::from_impl(VendorSource::new(addr, node, ctx).await?)),
            _ => error::NoSourceSnafu { kind }.fail().map_err(|e| e.into()),
        }
    }

    async fn create_transform(
        &self,
        addr: &Addr,
        node: &Node,
        ctx: &Context,
    ) -> PluginResult<Transform> {
        let kind = node.get_kind().context(error::NoKindSnafu)?;
        debug!(
            section = "core-plugin",
            component = "transform",
            "create transform {addr} with kind {kind}"
        );
        match kind.as_str() {
            "compose" => Ok(Transform::from_impl(
                ComposeTransform::new(addr, node, ctx).await?,
            )),
            "import" => Ok(Transform::from_impl(
                ImportTransform::new(addr, node, ctx).await?,
            )),
            "script" => Ok(Transform::from_impl(
                ScriptTransform::new(addr, node, ctx).await?,
            )),
            _ => error::NoTransformSnafu { kind }
                .fail()
                .map_err(|e| e.into()),
        }
    }

    async fn create_vendor(&self, addr: &Addr, node: &Node, ctx: &Context) -> PluginResult<Vendor> {
        let kind = node.get_kind().context(error::NoKindSnafu)?;
        match kind.as_str() {
            "image" => Ok(Vendor::from_impl(ImageVendor::new(addr, node, ctx).await?)),
            _ => error::NoVendorSnafu { kind }.fail().map_err(|e| e.into()),
        }
    }
}

pub mod error {
    use snafu::Snafu;

    use crate::plugin::error::PluginError;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        ContainerEnv {
            #[snafu(source(from(crate::std::environment::container::error::Error, Box::new)))]
            source: Box<crate::std::environment::container::error::Error>,
        },
        #[snafu(transparent)]
        Git {
            #[snafu(source(from(crate::std::source::git::error::Error, Box::new)))]
            source: Box<crate::std::source::git::error::Error>,
        },
        #[snafu(transparent)]
        LocalEnv {
            #[snafu(source(from(crate::std::environment::local::error::Error, Box::new)))]
            source: Box<crate::std::environment::local::error::Error>,
        },
        #[snafu(transparent)]
        LocalSource {
            #[snafu(source(from(crate::std::source::local::error::Error, Box::new)))]
            source: Box<crate::std::source::local::error::Error>,
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

    impl From<Error> for PluginError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }
}
