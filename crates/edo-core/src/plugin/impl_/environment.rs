use async_trait::async_trait;
use std::path::{Path, PathBuf};
use wasmtime::AsContextMut;

use super::handle::PluginHandle;
use crate::environment::{Command, Environment, EnvironmentImpl};
use crate::plugin::error;
use crate::storage::Id;
use crate::util::{Reader, Writer};
use crate::{
    context::Log,
    environment::{EnvResult, FarmImpl},
    storage::Storage,
};
use snafu::ResultExt;

pub struct PluginFarm(PluginHandle);
pub struct PluginEnvironment(PluginHandle);

impl PluginFarm {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

impl PluginEnvironment {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

#[async_trait]
impl FarmImpl for PluginFarm {
    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let storage = self_.push(storage.clone())?;
        let this = self_.handle.edo_plugin_abi().farm();
        let mut ctx = self_.store.lock();
        match this
            .call_setup(ctx.as_context_mut(), self_.me, log, storage)
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

    async fn create(&self, log: &Log, path: &Path) -> EnvResult<Environment> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().farm();
        let mut ctx = self_.store.lock();
        match this
            .call_create(ctx.as_context_mut(), self_.me, log, path.as_ref())
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(resource) => {
                drop(ctx);
                let handle = PluginHandle::new(self_.store.clone(), self_.handle.clone(), resource);
                Ok(Environment::from_impl(PluginEnvironment::new(handle)))
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }
}

#[async_trait]
impl EnvironmentImpl for PluginEnvironment {
    async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_expand(ctx.as_context_mut(), self_.me, path.as_ref())
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(PathBuf::from(result))
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn create_dir(&self, path: &Path) -> EnvResult<()> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_create_dir(ctx.as_context_mut(), self_.me, path.as_ref())
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

    async fn set_env(&self, key: &str, value: &str) -> EnvResult<()> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_set_env(ctx.as_context_mut(), self_.me, key, value)
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

    async fn get_env(&self, key: &str) -> Option<String> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        this.call_get_env(ctx.as_context_mut(), self_.me, key)
            .await
            .unwrap_or(None)
    }

    async fn setup(&self, log: &Log, storage: &Storage) -> EnvResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let storage = self_.push(storage.clone())?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_setup(ctx.as_context_mut(), self_.me, log, storage)
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

    async fn up(&self, log: &Log) -> EnvResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_up(ctx.as_context_mut(), self_.me, log)
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

    async fn down(&self, log: &Log) -> EnvResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_down(ctx.as_context_mut(), self_.me, log)
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

    async fn clean(&self, log: &Log) -> EnvResult<()> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_clean(ctx.as_context_mut(), self_.me, log)
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

    async fn write(&self, path: &Path, reader: Reader) -> EnvResult<()> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let reader = self_.push(reader)?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_write(ctx.as_context_mut(), self_.me, path.as_ref(), reader)
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

    async fn unpack(&self, path: &Path, reader: Reader) -> EnvResult<()> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let reader = self_.push(reader)?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_unpack(ctx.as_context_mut(), self_.me, path.as_ref(), reader)
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

    async fn read(&self, path: &Path, writer: Writer) -> EnvResult<()> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let writer = self_.push(writer)?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_read(ctx.as_context_mut(), self_.me, path.as_ref(), writer)
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

    async fn cmd(&self, log: &Log, id: &Id, path: &Path, command: &str) -> EnvResult<bool> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let id = self_.push(id.clone())?;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_cmd(
                ctx.as_context_mut(),
                self_.me,
                log,
                id,
                path.as_ref(),
                command,
            )
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

    async fn run(&self, log: &Log, id: &Id, path: &Path, command: &Command) -> EnvResult<bool> {
        let self_ = &self.0;
        let log = self_.push(log.clone())?;
        let id = self_.push(id.clone())?;
        let path = path.to_string_lossy();
        let command = self_.push(command.clone())?;
        let this = self_.handle.edo_plugin_abi().environment();
        let mut ctx = self_.store.lock();
        match this
            .call_run(
                ctx.as_context_mut(),
                self_.me,
                log,
                id,
                path.as_ref(),
                command,
            )
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

    fn shell(&self, path: &Path) -> EnvResult<()> {
        let self_ = &self.0;
        let path = path.to_string_lossy();
        let this = self_.handle.edo_plugin_abi().environment();
        let handle = tokio::runtime::Handle::current();
        let mut ctx = self_.store.lock();
        match handle
            .block_on(this.call_shell(ctx.as_context_mut(), self_.me, path.as_ref()))
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
