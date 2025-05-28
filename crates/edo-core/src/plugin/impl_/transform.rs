use crate::context::{Addr, Handle, Log};
use crate::environment::Environment;
use crate::plugin::bindings::edo::plugin::host;
use crate::plugin::error;
use crate::storage::Id;
use crate::transform::{transform_err, TransformImpl, TransformResult, TransformStatus};
use crate::util::sync_fn;
use async_trait::async_trait;
use snafu::ResultExt;
use std::path::PathBuf;
use wasmtime::AsContextMut;

use super::handle::PluginHandle;

pub struct PluginTransform(PluginHandle);

impl PluginTransform {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

#[async_trait]
impl TransformImpl for PluginTransform {
    async fn get_unique_id(&self, context: &Handle) -> TransformResult<Id> {
        let self_ = &self.0;
        let context = self_.push(context.clone())?;
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match this
            .call_get_unique_id(ctx.as_context_mut(), self_.me, context)
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

    async fn environment(&self) -> TransformResult<Addr> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match this
            .call_environment(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(addr) => {
                drop(ctx);
                let result = Addr::parse(addr.as_str())?;
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match this
            .call_depends(ctx.as_context_mut(), self_.me)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(list) => {
                drop(ctx);
                let mut result = Vec::new();
                for entry in list.iter() {
                    result.push(Addr::parse(entry)?);
                }
                Ok(result)
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }
    async fn prepare(&self, log: &Log, context: &Handle) -> TransformResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let context = self_.push(context.clone())?;
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match this
            .call_prepare(ctx.as_context_mut(), self_.me, log, context)
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

    async fn stage(&self, log: &Log, context: &Handle, env: &Environment) -> TransformResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let context = self_.push(context.clone())?;

        let env = self_.push(env.clone())?;
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match this
            .call_stage(ctx.as_context_mut(), self_.me, log, context, env)
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

    async fn transform(&self, log: &Log, context: &Handle, env: &Environment) -> TransformStatus {
        let self_ = &self.0;
        let log = transform_err!(self_.push(log.clone()));
        let context = transform_err!(self_.push(context.clone()));
        let env = transform_err!(self_.push(env.clone()));
        let this = self_.handle.edo_plugin_abi().transform();
        let mut ctx = self_.store.lock();
        match transform_err!(this
            .call_transform(ctx.as_context_mut(), self_.me, log, context, env)
            .await
            .context(error::WasmExecSnafu))
        {
            host::TransformStatus::Success(artifact) => {
                drop(ctx);
                let artifact = transform_err!(self_.get(&artifact));
                TransformStatus::Success(artifact)
            }
            host::TransformStatus::Retryable((path, error)) => {
                drop(ctx);
                let error = transform_err!(self_.get(&error));
                TransformStatus::Retryable(
                    path.map(PathBuf::from),
                    error::PluginError::Guest { guest: error }.into(),
                )
            }
            host::TransformStatus::Failed((path, error)) => {
                drop(ctx);
                let error = transform_err!(self_.get(&error));
                TransformStatus::Failed(
                    path.map(PathBuf::from),
                    error::PluginError::Guest { guest: error }.into(),
                )
            }
        }
    }

    fn can_shell(&self) -> bool {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().transform();
        sync_fn(async move || {
            this.call_can_shell(self_.store.lock().as_context_mut(), self_.me)
                .await
                .unwrap_or(false)
        })
    }

    fn shell(&self, env: &Environment) -> TransformResult<()> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().transform();
        sync_fn(async move || {
            let env = self_.push(env.clone())?;
            let mut ctx = self_.store.lock();
            match this
                .call_shell(ctx.as_context_mut(), self_.me, env)
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
        })
    }
}
