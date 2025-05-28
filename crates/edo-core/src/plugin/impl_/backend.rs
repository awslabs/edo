use std::collections::BTreeSet;

use async_trait::async_trait;
use wasmtime::AsContextMut;

use super::handle::PluginHandle;
use crate::plugin::error;
use crate::{
    storage::{Artifact, BackendImpl, Id, Layer, MediaType, StorageResult},
    util::{Reader, Writer},
};
use edo_oci::models::Platform;
use snafu::ResultExt;

pub struct PluginBackend(PluginHandle);

impl PluginBackend {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

#[async_trait]
impl BackendImpl for PluginBackend {
    async fn list(&self) -> StorageResult<BTreeSet<Id>> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_ls(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(data) => {
                drop(ctx);
                let mut found = BTreeSet::new();
                for id in data.iter() {
                    found.insert(self_.get(id)?);
                }
                Ok(found)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn has(&self, id: &Id) -> StorageResult<bool> {
        let self_ = &self.0;
        let id = self_.push(id.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_has(ctx.as_context_mut(), self_.me, id)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(flag) => {
                drop(ctx);
                Ok(flag)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn open(&self, id: &Id) -> StorageResult<Artifact> {
        let self_ = &self.0;
        let id = self_.push(id.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_open(ctx.as_context_mut(), self_.me, id)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(item) => {
                drop(ctx);
                let result = self_.get(&item)?;
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn save(&self, artifact: &Artifact) -> StorageResult<()> {
        let self_ = &self.0;
        let artifact = self_.push(artifact.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_save(ctx.as_context_mut(), self_.me, artifact)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn del(&self, id: &Id) -> StorageResult<()> {
        let self_ = &self.0;
        let id = self_.push(id.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_del(ctx.as_context_mut(), self_.me, id)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn copy(&self, from: &Id, to: &Id) -> StorageResult<()> {
        let self_ = &self.0;
        let from = self_.push(from.clone())?;
        let to = self_.push(to.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_copy(ctx.as_context_mut(), self_.me, from, to)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn prune(&self, id: &Id) -> StorageResult<()> {
        let self_ = &self.0;
        let id = self_.push(id.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_prune(ctx.as_context_mut(), self_.me, id)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn prune_all(&self) -> StorageResult<()> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_prune_all(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn read(&self, layer: &Layer) -> StorageResult<Reader> {
        let self_ = &self.0;
        let layer = self_.push(layer.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_read(ctx.as_context_mut(), self_.me, layer)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(reader) => {
                drop(ctx);
                let result = self_.get(&reader)?;
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn start_layer(&self) -> StorageResult<Writer> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_start_layer(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(writer) => {
                drop(ctx);
                let result = self_.get(&writer)?;
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn finish_layer(
        &self,
        media_type: &MediaType,
        platform: Option<Platform>,
        writer: &Writer,
    ) -> StorageResult<Layer> {
        let self_ = &self.0;
        let media_type = media_type.to_string();
        let platform = platform.map(|x| x.to_string());
        let writer = self_.push(writer.clone())?;
        let this = self_.handle.edo_plugin_abi().backend();
        let mut ctx = self_.store.lock();
        match this
            .call_finish_layer(
                ctx.as_context_mut(),
                self_.me,
                media_type.as_str(),
                if let Some(platform) = platform.as_ref() {
                    Some(platform.as_str())
                } else {
                    None
                },
                writer,
            )
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(layer) => {
                drop(ctx);
                let result = self_.get(&layer)?;
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }
}
