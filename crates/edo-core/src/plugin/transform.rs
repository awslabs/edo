use wasmtime::component::Resource;

use crate::{
    context::{Handle, Log},
    environment::Environment,
    storage::Id,
    transform::{Transform, TransformStatus},
};

use super::{
    bindings::edo::plugin::host,
    error::{self, wasm_ok},
    host::Host,
    WasmResult,
};

impl host::HostTransform for Host {
    async fn environment(&mut self, self_: Resource<Transform>) -> wasmtime::Result<String> {
        let transform = self.table.get(&self_)?;
        Ok(transform.environment().await?.to_string())
    }

    async fn depends(&mut self, self_: Resource<Transform>) -> WasmResult<Vec<String>> {
        let transform = self.table.get(&self_)?;
        let result = wasm_ok!(with self.table => transform.depends().await;
        with result {
            Ok(result.iter().map(|x| x.to_string()).collect())
        });
        Ok(result)
    }

    async fn get_unique_id(
        &mut self,
        self_: Resource<Transform>,
        ctx: Resource<Handle>,
    ) -> WasmResult<Resource<Id>> {
        let transform = self.table.get(&self_)?;
        let ctx = self.table.get(&ctx)?;
        let result = wasm_ok!(with self.table => transform.get_unique_id(ctx).await;
        with result {
            let resource = self.table.push(result).unwrap();
            Ok(resource)
        });
        Ok(result)
    }

    async fn prepare(
        &mut self,
        self_: Resource<Transform>,
        log: Resource<Log>,
        ctx: Resource<Handle>,
    ) -> WasmResult<()> {
        let transform = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let ctx = self.table.get(&ctx)?;
        let _ = wasm_ok!(with self.table => transform.prepare(log, ctx).await);
        Ok(Ok(()))
    }

    async fn stage(
        &mut self,
        self_: Resource<Transform>,
        log: Resource<Log>,
        ctx: Resource<Handle>,
        env: Resource<Environment>,
    ) -> WasmResult<()> {
        let transform = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let ctx = self.table.get(&ctx)?;
        let env = self.table.get(&env)?;
        let _ = wasm_ok!(with self.table => transform.stage(log, ctx, env).await);
        Ok(Ok(()))
    }

    async fn transform(
        &mut self,
        self_: Resource<Transform>,
        log: Resource<Log>,
        ctx: Resource<Handle>,
        env: Resource<Environment>,
    ) -> wasmtime::Result<host::TransformStatus> {
        let transform = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let ctx = self.table.get(&ctx)?;
        let env = self.table.get(&env)?;
        let result = match transform.transform(log, ctx, env).await {
            TransformStatus::Success(artifact) => {
                let handle = self.table.push(artifact)?;
                host::TransformStatus::Success(handle)
            }
            TransformStatus::Failed(path, error) => host::TransformStatus::Failed((
                path.map(|x| x.to_string_lossy().to_string()),
                self.table.push(error::GuestError {
                    plugin: "host".to_string(),
                    message: error.to_string(),
                })?,
            )),
            TransformStatus::Retryable(path, error) => host::TransformStatus::Retryable((
                path.map(|x| x.to_string_lossy().to_string()),
                self.table.push(error::GuestError {
                    plugin: "host".to_string(),
                    message: error.to_string(),
                })?,
            )),
        };
        Ok(result)
    }

    async fn can_shell(&mut self, self_: Resource<Transform>) -> wasmtime::Result<bool> {
        let transform = self.table.get(&self_)?;
        Ok(transform.can_shell())
    }

    async fn shell(
        &mut self,
        self_: Resource<Transform>,
        env: Resource<Environment>,
    ) -> WasmResult<()> {
        let transform = self.table.get(&self_)?;
        let env = self.table.get(&env)?;
        let result = wasm_ok!(with self.table => transform.shell(env));
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Transform>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
