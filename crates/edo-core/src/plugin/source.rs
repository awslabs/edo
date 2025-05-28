use crate::{
    context::{Addr, Context, Log, Node},
    environment::Environment,
    source::Source,
    storage::{Artifact, Id, Storage},
};
use std::path::Path;
use wasmtime::component::Resource;

use super::{bindings::edo::plugin::host, error::wasm_ok, host::Host, WasmResult};

impl host::HostSource for Host {
    async fn new(
        &mut self,
        addr: String,
        node: Resource<Node>,
        ctx: Resource<Context>,
    ) -> wasmtime::Result<Resource<Source>> {
        let node = self.table.get(&node)?;
        let ctx = self.table.get(&ctx)?;
        let addr = Addr::parse(addr.as_str()).unwrap();
        let source = ctx.add_source(&addr, node).await?;
        let result = self.table.push(source)?;
        Ok(result)
    }

    async fn get_unique_id(&mut self, self_: Resource<Source>) -> WasmResult<Resource<Id>> {
        let source = self.table.get(&self_)?;
        let result = wasm_ok!(with self.table => source.get_unique_id().await;
        with result {
            let resource = self.table.push(result).unwrap();
            Ok(resource)
        });
        Ok(result)
    }

    async fn cache(
        &mut self,
        self_: Resource<Source>,
        log: Resource<Log>,
        storage: Resource<Storage>,
    ) -> WasmResult<Resource<Artifact>> {
        let this = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let storage = self.table.get(&storage)?;
        let result = wasm_ok! {
            with self.table => this.cache(log, storage).await;
            with result {
                let handle = self.table.push(result).unwrap();
                Ok(handle)
            }
        };
        Ok(result)
    }

    async fn fetch(
        &mut self,
        self_: Resource<Source>,
        log: Resource<Log>,
        storage: Resource<Storage>,
    ) -> WasmResult<Resource<Artifact>> {
        let source = self.table.get(&self_)?;
        let storage = self.table.get(&storage)?;
        let log = self.table.get(&log)?;
        let result = wasm_ok! {
            with self.table => source.fetch(log, storage).await;
            with result {
                let resource = self.table.push(result).unwrap();
                Ok(resource)
            }
        };
        Ok(result)
    }

    async fn stage(
        &mut self,
        self_: Resource<Source>,
        log: Resource<Log>,
        storage: Resource<Storage>,
        env: Resource<Environment>,
        path: String,
    ) -> WasmResult<()> {
        let source = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let storage = self.table.get(&storage)?;
        let env = self.table.get(&env)?;
        let _ = wasm_ok! {
            with self.table => source.stage(log, storage, env, Path::new(path.as_str())).await
        };
        Ok(Ok(()))
    }

    async fn drop(&mut self, self_: Resource<Source>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
