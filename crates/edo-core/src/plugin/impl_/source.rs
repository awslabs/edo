use std::path::Path;

use async_trait::async_trait;
use wasmtime::AsContextMut;

use super::handle::PluginHandle;
use crate::{
    context::Log,
    environment::Environment,
    plugin::error,
    source::{SourceImpl, SourceResult},
    storage::{Artifact, Id, Storage},
};
use snafu::ResultExt;

pub struct PluginSource(PluginHandle);

impl PluginSource {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

#[async_trait]
impl SourceImpl for PluginSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().source();
        let mut ctx = self_.store.lock();
        match this
            .call_get_unique_id(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(resource) => {
                drop(ctx);
                self_.get(&resource).map_err(|e| e.into())
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let storage = self_.push(storage.clone())?;
        let this = self_.handle.edo_plugin_abi().source();
        let mut ctx = self_.store.lock();
        match this
            .call_fetch(ctx.as_context_mut(), self_.me, log, storage)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(resource) => {
                drop(ctx);
                self_.get(&resource).map_err(|e| e.into())
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn stage(
        &self,
        log: &Log,
        storage: &Storage,
        env: &Environment,
        path: &Path,
    ) -> SourceResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let storage = self_.push(storage.clone())?;
        let env = self_.push(env.clone())?;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().source();
        let mut ctx = self_.store.lock();
        match this
            .call_stage(
                ctx.as_context_mut(),
                self_.me,
                log,
                storage,
                env,
                path.as_ref(),
            )
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(()) => {
                drop(ctx);
                Ok(())
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }
}
