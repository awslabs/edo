use std::path::Path;

use super::{bindings::edo::plugin::host, error::wasm_ok, host::Host, WasmResult};
use crate::{
    util::{Reader, Writer},
    {
        context::Log,
        environment::{Command, Environment, Farm},
        storage::{Id, Storage},
    },
};
use wasmtime::component::Resource;

impl host::HostCommand for Host {
    async fn set(
        &mut self,
        self_: Resource<Command>,
        key: String,
        value: String,
    ) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.set(key.as_str(), value.as_str()));
        Ok(result)
    }

    async fn chdir(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.chdir(path.as_str()));
        Ok(result)
    }

    async fn pushd(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.pushd(path.as_str()));
        Ok(result)
    }

    async fn popd(&mut self, self_: Resource<Command>) -> wasmtime::Result<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        this.popd();
        Ok(())
    }

    async fn create_named_dir(
        &mut self,
        self_: Resource<Command>,
        key: String,
        path: String,
    ) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result =
            wasm_ok!(with self.table => this.create_named_dir(key.as_str(), path.as_str()).await);
        Ok(result)
    }

    async fn create_dir(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.create_dir(path.as_str()).await);
        Ok(result)
    }

    async fn remove_dir(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.remove_dir(path.as_str()).await);
        Ok(result)
    }

    async fn remove_file(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.remove_file(path.as_str()).await);
        Ok(result)
    }

    async fn mv(&mut self, self_: Resource<Command>, from: String, to: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.mv(from.as_str(), to.as_str()).await);
        Ok(result)
    }

    async fn copy(&mut self, self_: Resource<Command>, from: String, to: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.copy(from.as_str(), to.as_str()).await);
        Ok(result)
    }

    async fn run(&mut self, self_: Resource<Command>, cmd: String) -> WasmResult<()> {
        let this: &mut Command = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => this.run(cmd.as_str()).await);
        Ok(result)
    }

    async fn send(&mut self, self_: Resource<Command>, path: String) -> WasmResult<()> {
        let this = self.table.get(&self_)?;
        let result = wasm_ok!(with self.table => this.send(path.as_str()).await);
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Command>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostFarm for Host {
    async fn setup(
        &mut self,
        self_: Resource<Farm>,
        log: Resource<Log>,
        storage: Resource<Storage>,
    ) -> WasmResult<()> {
        let this = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let storage = self.table.get(&storage)?;
        let result = wasm_ok!(with self.table => this.setup(log, storage).await);
        Ok(result)
    }

    async fn create(
        &mut self,
        self_: Resource<Farm>,
        log: Resource<Log>,
        path: String,
    ) -> WasmResult<Resource<Environment>> {
        let this = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let result = wasm_ok!(with self.table => this.create(log, Path::new(path.as_str())).await;
        with result {
            let item = self.table.push(result).unwrap();
            Ok(item)
        });
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Farm>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostEnvironment for Host {
    async fn defer_cmd(
        &mut self,
        self_: Resource<Environment>,
        log: Resource<Log>,
        id: Resource<Id>,
    ) -> wasmtime::Result<Resource<Command>> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let id = self.table.get(&id)?;
        let sandbox = env.defer_cmd(log, id);
        let result = self.table.push(sandbox)?;
        Ok(result)
    }

    async fn expand(&mut self, self_: Resource<Environment>, path: String) -> WasmResult<String> {
        let env = self.table.get(&self_)?;
        let result = wasm_ok! {
            with self.table => env.expand(Path::new(path.as_str())).await;
            with result {
                Ok(result.to_str().unwrap().to_string())
            }
        };
        Ok(result)
    }

    async fn create_dir(&mut self, self_: Resource<Environment>, path: String) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let _ = wasm_ok! {
            with self.table => env.create_dir(Path::new(path.as_str())).await
        };
        Ok(Ok(()))
    }

    async fn set_env(
        &mut self,
        self_: Resource<Environment>,
        key: String,
        value: String,
    ) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let _ = wasm_ok!(with self.table => env.set_env(key.as_str(), value.as_str()).await);
        Ok(Ok(()))
    }

    async fn get_env(
        &mut self,
        self_: Resource<Environment>,
        key: String,
    ) -> wasmtime::Result<Option<String>> {
        let env = self.table.get(&self_)?;
        Ok(env.get_env(key.as_str()).await)
    }

    async fn setup(
        &mut self,
        self_: Resource<Environment>,
        log: Resource<Log>,
        storage: Resource<Storage>,
    ) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let storage = self.table.get(&storage)?;
        let _ = wasm_ok!(with self.table => env.setup(log, storage).await);
        Ok(Ok(()))
    }

    async fn up(&mut self, self_: Resource<Environment>, log: Resource<Log>) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let _ = wasm_ok!(with self.table => env.up(log).await);
        Ok(Ok(()))
    }

    async fn down(&mut self, self_: Resource<Environment>, log: Resource<Log>) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let _ = wasm_ok!(with self.table => env.down(log).await);
        Ok(Ok(()))
    }

    async fn clean(&mut self, self_: Resource<Environment>, log: Resource<Log>) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let _ = wasm_ok!(with self.table => env.clean(log).await);
        Ok(Ok(()))
    }

    async fn write(
        &mut self,
        self_: Resource<Environment>,
        path: String,
        data: Resource<Reader>,
    ) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let reader = self.table.get(&data)?;
        let _ =
            wasm_ok!(with self.table => env.write(Path::new(path.as_str()), reader.clone()).await);
        Ok(Ok(()))
    }

    async fn unpack(
        &mut self,
        self_: Resource<Environment>,
        path: String,
        data: Resource<Reader>,
    ) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let reader = self.table.get(&data)?;
        let _ =
            wasm_ok!(with self.table => env.unpack(Path::new(path.as_str()), reader.clone()).await);
        Ok(Ok(()))
    }

    async fn read(
        &mut self,
        self_: Resource<Environment>,
        path: String,
        writer: Resource<Writer>,
    ) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let writer = self.table.get(&writer)?;
        let result = wasm_ok! {
            with self.table => env.read(Path::new(path.as_str()), writer.clone()).await;
            with result {
                Ok(())
            }
        };
        Ok(result)
    }

    async fn cmd(
        &mut self,
        self_: Resource<Environment>,
        log: Resource<Log>,
        id: Resource<Id>,
        path: String,
        cmd: String,
    ) -> WasmResult<bool> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let id = self.table.get(&id)?;
        let result = wasm_ok! {
            with self.table => env.cmd(log, id, Path::new(path.as_str()), cmd.as_str()).await
        };
        Ok(result)
    }

    async fn run(
        &mut self,
        self_: Resource<Environment>,
        log: Resource<Log>,
        id: Resource<Id>,
        path: String,
        command: Resource<Command>,
    ) -> WasmResult<bool> {
        let env = self.table.get(&self_)?;
        let log = self.table.get(&log)?;
        let id = self.table.get(&id)?;
        let script = self.table.get(&command)?;
        let result = wasm_ok! {
            with self.table => env.run(log, id, Path::new(path.as_str()), script).await
        };
        Ok(result)
    }

    async fn shell(&mut self, self_: Resource<Environment>, path: String) -> WasmResult<()> {
        let env = self.table.get(&self_)?;
        let result = wasm_ok! {
            with self.table => env.shell(Path::new(path.as_str()))
        };
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Environment>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
