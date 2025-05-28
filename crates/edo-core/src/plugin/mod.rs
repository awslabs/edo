use async_trait::async_trait;
use impl_::backend::PluginBackend;
use impl_::environment::PluginFarm;
use impl_::handle::PluginHandle;
use impl_::source::PluginSource;
use impl_::transform::PluginTransform;
use impl_::vendor::PluginVendor;
use parking_lot::Mutex;
use snafu::OptionExt;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use wasmtime::{AsContextMut, component::Resource};

pub mod bindings;
pub mod environment;
pub mod error;
pub mod host;
pub mod log;
pub mod node;
pub mod source;
pub mod storage;
pub mod transform;

mod impl_;

use crate::context::{Addr, Context, Definable, FromNode, Log, Node};
use crate::def_trait;
use crate::environment::Farm;
use crate::non_configurable;
use crate::source::Source;
use crate::{
    source::Vendor,
    storage::{Backend, Storage},
    transform::Transform,
};
use snafu::ResultExt;
use wasmtime::{
    Engine, Store,
    component::{Component, Linker},
};

pub type Result<T> = std::result::Result<T, error::PluginError>;
type WasmResult<T> = wasmtime::Result<std::result::Result<T, Resource<error::GuestError>>>;

def_trait! {
    "Defines the interface plugin implementations should follow" =>
    "A plugin is a group of sources, environments and transforms" =>
    Plugin: PluginImpl {
        "Fetch anything required for this plugin to operate" =>
        fetch(log: &Log, storage: &Storage) -> Result<()>;
        "Run any setup steps required for this plugin" =>
        setup(log: &Log, storage: &Storage) -> Result<()>;
        "Create a storage backend using this plugin" =>
        create_storage(addr: &Addr, node: &Node, config: &Context) -> Result<Backend>;
        "Create an environment farm using this plugin" =>
        create_farm(addr: &Addr, node: &Node, ctx: &Context) -> Result<Farm>;
        "Create a source using this plugin" =>
        create_source(addr: &Addr, node: &Node, ctx: &Context) -> Result<Source>;
        "Create a transform using this plugin" =>
        create_transform(addr: &Addr, node: &Node, ctx: &Context) -> Result<Transform>;
        "Create a vendor using this plugin" =>
        create_vendor(addr: &Addr, node: &Node, ctx: &Context) -> Result<Vendor>
    }
}

#[async_trait]
impl FromNode for Plugin {
    type Error = error::PluginError;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self> {
        let kind = node.get_kind().unwrap();
        match kind.as_str() {
            "wasm" => Ok(Plugin::from_impl(WasmPlugin::new(addr, node, ctx).await?)),
            value => error::UnknownSnafu { kind: value }.fail(),
        }
    }
}

non_configurable!(Plugin, error::PluginError);

#[derive(Clone)]
pub struct WasmPlugin {
    source: Source,
}

#[async_trait]
impl FromNode for WasmPlugin {
    type Error = error::PluginError;

    async fn from_node(addr: &Addr, node: &Node, ctx: &Context) -> Result<Self> {
        let source_node = node
            .get("source")
            .or(node.get("wants"))
            .context(error::SourceRequiredSnafu)?;
        let source = source_node
            .as_list()
            .and_then(|x| x.first().cloned())
            .unwrap();
        let source = ctx.add_source(addr, &source).await?;
        Ok(Self { source })
    }
}

non_configurable!(WasmPlugin, error::PluginError);

impl WasmPlugin {
    async fn load(
        &self,
        storage: &Storage,
    ) -> Result<(Arc<bindings::Edo>, Arc<Mutex<Store<host::Host>>>)> {
        // First we need to get the artifact
        let id = self.source.get_unique_id().await?;
        let artifact = storage.safe_open(&id).await?;
        // A plugin artifact should only have one layer :D
        let mut reader = storage
            .safe_read(artifact.layers().first().unwrap())
            .await?;
        let mut buffer = Vec::new();
        reader
            .read_to_end(&mut buffer)
            .await
            .context(error::IoSnafu)?;

        let engine = Engine::new(wasmtime::Config::default().async_support(true))
            .context(error::WasmExecSnafu)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).context(error::WasmExecSnafu)?;
        bindings::Edo::add_to_linker(&mut linker, |state: &mut host::Host| state)
            .context(error::WasmExecSnafu)?;
        let mut store = Store::new(&engine, host::Host::new());
        let component =
            Component::from_binary(&engine, buffer.as_slice()).context(error::WasmExecSnafu)?;

        // Create the handle
        let handle = Arc::new(
            bindings::Edo::instantiate_async(&mut store, &component, &linker)
                .await
                .context(error::WasmExecSnafu)?,
        );
        let store = Arc::new(Mutex::new(store));
        Ok((handle, store))
    }
}

#[async_trait]
impl PluginImpl for WasmPlugin {
    async fn fetch(&self, log: &Log, storage: &Storage) -> Result<()> {
        self.source.cache(log, storage).await?;
        Ok(())
    }

    async fn setup(&self, _log: &Log, _storage: &Storage) -> Result<()> {
        Ok(())
    }

    async fn create_storage(&self, addr: &Addr, node: &Node, ctx: &Context) -> Result<Backend> {
        let (handle, store) = self.load(ctx.storage()).await?;
        let store_ref = store.clone();
        let mut store = store_ref.lock();
        let addr = addr.to_string();
        let node_ref = store
            .data_mut()
            .table
            .push(node.clone())
            .context(error::WasmContextSnafu)?;
        let context = store
            .data_mut()
            .table
            .push(ctx.clone())
            .context(error::WasmContextSnafu)?;
        let farm = match handle
            .edo_plugin_abi()
            .call_create_storage(store.as_context_mut(), addr.as_str(), node_ref, context)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let guest = store
                    .data_mut()
                    .table
                    .get(&e)
                    .context(error::WasmContextSnafu)?;
                error::GuestSnafu {
                    guest: guest.clone(),
                }
                .fail()
            }
        }?;
        Ok(Backend::from_impl(PluginBackend::new(PluginHandle::new(
            store_ref.clone(),
            handle.clone(),
            farm,
        ))))
    }

    async fn create_farm(&self, addr: &Addr, node: &Node, ctx: &Context) -> Result<Farm> {
        let (handle, store) = self.load(ctx.storage()).await?;
        let store_ref = store.clone();
        let mut store = store_ref.lock();
        let addr = addr.to_string();
        let node_ref = store
            .data_mut()
            .table
            .push(node.clone())
            .context(error::WasmContextSnafu)?;
        let context = store
            .data_mut()
            .table
            .push(ctx.clone())
            .context(error::WasmContextSnafu)?;
        let farm = match handle
            .edo_plugin_abi()
            .call_create_farm(store.as_context_mut(), addr.as_str(), node_ref, context)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let guest = store
                    .data_mut()
                    .table
                    .get(&e)
                    .context(error::WasmContextSnafu)?;
                error::GuestSnafu {
                    guest: guest.clone(),
                }
                .fail()
            }
        }?;
        Ok(Farm::from_impl(PluginFarm::new(PluginHandle::new(
            store_ref.clone(),
            handle.clone(),
            farm,
        ))))
    }

    async fn create_source(&self, addr: &Addr, node: &Node, ctx: &Context) -> Result<Source> {
        let (handle, store) = self.load(ctx.storage()).await?;
        let store_ref = store.clone();
        let mut store = store_ref.lock();
        let addr = addr.to_string();
        let node_ref = store
            .data_mut()
            .table
            .push(node.clone())
            .context(error::WasmContextSnafu)?;
        let context = store
            .data_mut()
            .table
            .push(ctx.clone())
            .context(error::WasmContextSnafu)?;
        let transform = match handle
            .edo_plugin_abi()
            .call_create_source(store.as_context_mut(), addr.as_str(), node_ref, context)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let guest = store
                    .data_mut()
                    .table
                    .get(&e)
                    .context(error::WasmContextSnafu)?;
                error::GuestSnafu {
                    guest: guest.clone(),
                }
                .fail()
            }
        }?;
        Ok(Source::from_impl(PluginSource::new(PluginHandle::new(
            store_ref.clone(),
            handle.clone(),
            transform,
        ))))
    }

    async fn create_transform(&self, addr: &Addr, node: &Node, ctx: &Context) -> Result<Transform> {
        let (handle, store) = self.load(ctx.storage()).await?;
        let store_ref = store.clone();
        let mut store = store_ref.lock();
        let addr = addr.to_string();
        let node_ref = store
            .data_mut()
            .table
            .push(node.clone())
            .context(error::WasmContextSnafu)?;
        let context = store
            .data_mut()
            .table
            .push(ctx.clone())
            .context(error::WasmContextSnafu)?;
        let transform = match handle
            .edo_plugin_abi()
            .call_create_transform(store.as_context_mut(), addr.as_str(), node_ref, context)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let guest = store
                    .data_mut()
                    .table
                    .get(&e)
                    .context(error::WasmContextSnafu)?;
                error::GuestSnafu {
                    guest: guest.clone(),
                }
                .fail()
            }
        }?;
        Ok(Transform::from_impl(PluginTransform::new(
            PluginHandle::new(store_ref.clone(), handle.clone(), transform),
        )))
    }

    async fn create_vendor(&self, addr: &Addr, node: &Node, ctx: &Context) -> Result<Vendor> {
        let (handle, store) = self.load(ctx.storage()).await?;
        let store_ref = store.clone();
        let mut store = store_ref.lock();
        let addr = addr.to_string();
        let node_ref = store
            .data_mut()
            .table
            .push(node.clone())
            .context(error::WasmContextSnafu)?;
        let context = store
            .data_mut()
            .table
            .push(ctx.clone())
            .context(error::WasmContextSnafu)?;
        let transform = match handle
            .edo_plugin_abi()
            .call_create_vendor(store.as_context_mut(), addr.as_str(), node_ref, context)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let guest = store
                    .data_mut()
                    .table
                    .get(&e)
                    .context(error::WasmContextSnafu)?;
                error::GuestSnafu {
                    guest: guest.clone(),
                }
                .fail()
            }
        }?;
        Ok(Vendor::from_impl(PluginVendor::new(PluginHandle::new(
            store_ref.clone(),
            handle.clone(),
            transform,
        ))))
    }
}
