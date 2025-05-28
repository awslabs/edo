use wasmtime::component::Resource;
use wasmtime_wasi::p2::{IoView, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi::ResourceTable;

use crate::{
    context::{Addr, Config, Context, Handle, Node},
    environment::Farm,
    source::Source,
    storage::Storage,
    transform::Transform,
};

use super::{
    bindings::edo::plugin::host,
    error::{wasm_ok, GuestError},
    WasmResult,
};

pub struct Host {
    wasi: WasiCtx,
    pub(crate) table: ResourceTable,
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    pub fn new() -> Self {
        let wasi = WasiCtxBuilder::new().inherit_stdout().build();
        Self {
            wasi,
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

impl IoView for Host {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl host::HostError for Host {
    async fn new(
        &mut self,
        plugin: String,
        message: String,
    ) -> wasmtime::Result<Resource<GuestError>> {
        let error = GuestError { plugin, message };
        self.table.push(error).map_err(|e| e.into())
    }

    async fn to_string(&mut self, self_: Resource<GuestError>) -> wasmtime::Result<String> {
        let error = self.table.get(&self_)?;
        Ok(error.to_string())
    }

    async fn drop(&mut self, self_: Resource<GuestError>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::Host for Host {
    async fn info(&mut self, message: String) -> wasmtime::Result<()> {
        info!(target: "plugin", "{}", message);
        Ok(())
    }

    async fn warn(&mut self, message: String) -> wasmtime::Result<()> {
        warn!(target: "plugin", "{}", message);
        Ok(())
    }

    async fn fatal(&mut self, message: String) -> wasmtime::Result<()> {
        error!(target: "plugin", "{}", message);
        Ok(())
    }
}

impl host::HostHandle for Host {
    async fn storage(&mut self, self_: Resource<Handle>) -> wasmtime::Result<Resource<Storage>> {
        let this = self.table.get(&self_)?;
        let storage = this.storage();
        let handle = self.table.push(storage.clone())?;
        Ok(handle)
    }

    async fn get(
        &mut self,
        self_: Resource<Handle>,
        addr: String,
    ) -> wasmtime::Result<Option<Resource<Transform>>> {
        let this = self.table.get(&self_)?;
        let addr = Addr::parse(addr.as_str())?;
        if let Some(transform) = this.get(&addr) {
            let item = self.table.push(transform)?;
            Ok(Some(item))
        } else {
            Ok(None)
        }
    }

    async fn drop(&mut self, self_: Resource<Handle>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostContext for Host {
    async fn get_arg(
        &mut self,
        self_: Resource<Context>,
        name: String,
    ) -> wasmtime::Result<Option<String>> {
        let this = self.table.get(&self_)?;
        let result = this.args().get(&name).cloned();
        Ok(result)
    }

    async fn get_handle(&mut self, self_: Resource<Context>) -> wasmtime::Result<Resource<Handle>> {
        let this = self.table.get(&self_)?;
        let result = this.get_handle();
        let handle = self.table.push(result)?;
        Ok(handle)
    }

    async fn config(&mut self, self_: Resource<Context>) -> wasmtime::Result<Resource<Config>> {
        let this = self.table.get(&self_)?;
        let result = this.config();
        let handle = self.table.push(result.clone())?;
        Ok(handle)
    }

    async fn storage(&mut self, self_: Resource<Context>) -> wasmtime::Result<Resource<Storage>> {
        let this = self.table.get(&self_)?;
        let result = this.storage();
        let handle = self.table.push(result.clone())?;
        Ok(handle)
    }

    async fn get_transform(
        &mut self,
        self_: Resource<Context>,
        addr: String,
    ) -> wasmtime::Result<Option<Resource<Transform>>> {
        let this = self.table.get(&self_)?;
        let addr = Addr::parse(addr.as_str())?;
        if let Some(transform) = this.get_transform(&addr) {
            let handle = self.table.push(transform.clone())?;
            Ok(Some(handle))
        } else {
            Ok(None)
        }
    }

    async fn get_farm(
        &mut self,
        self_: Resource<Context>,
        addr: String,
    ) -> wasmtime::Result<Option<Resource<Farm>>> {
        let this = self.table.get(&self_)?;
        let addr = Addr::parse(addr.as_str())?;
        if let Some(farm) = this.get_farm(&addr) {
            let handle = self.table.push(farm.clone())?;
            Ok(Some(handle))
        } else {
            Ok(None)
        }
    }

    async fn add_source(
        &mut self,
        self_: Resource<Context>,
        addr: String,
        node: Resource<Node>,
    ) -> WasmResult<Resource<Source>> {
        let this = self.table.get(&self_)?;
        let addr = Addr::parse(addr.as_str())?;
        let node = self.table.get(&node)?;
        let result = wasm_ok!(
            with self.table => this.add_source(&addr, node).await;
            with result {
                let handle = self.table.push(result).unwrap();
                Ok(handle)
        });
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Context>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostConfig for Host {
    async fn get(
        &mut self,
        self_: Resource<Config>,
        name: String,
    ) -> wasmtime::Result<Option<Resource<Node>>> {
        let config = self.table.get(&self_)?;
        if let Some(item) = config.get(name.as_str()) {
            Ok(Some(self.table.push(item)?))
        } else {
            Ok(None)
        }
    }

    async fn drop(&mut self, self_: Resource<Config>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
