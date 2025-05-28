use wasmtime::component::Resource;

use crate::context::Node;

use super::{bindings::edo::plugin::host, error::wasm_ok, host::Host, WasmResult};

impl host::HostNode for Host {
    async fn validate_keys(&mut self, self_: Resource<Node>, keys: Vec<String>) -> WasmResult<()> {
        let node = self.table.get(&self_)?;
        let _ = wasm_ok!(with self.table => node.validate_keys(
            keys.iter()
                .map(|x| x.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        ));
        Ok(Ok(()))
    }

    async fn as_bool(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<bool>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_bool())
    }

    async fn as_int(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<i64>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_int())
    }

    async fn as_float(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<f64>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_float())
    }

    async fn as_string(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_string())
    }

    async fn as_version(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_version().map(|x| x.to_string()))
    }

    async fn as_require(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.as_require().map(|x| x.to_string()))
    }

    async fn as_list(
        &mut self,
        self_: Resource<Node>,
    ) -> wasmtime::Result<Option<Vec<Resource<Node>>>> {
        let node = self.table.get(&self_)?;
        if let Some(children) = node.as_list() {
            let mut nodes = Vec::new();
            for child in children {
                nodes.push(self.table.push(child)?);
            }
            Ok(Some(nodes))
        } else {
            Ok(None)
        }
    }

    async fn as_table(
        &mut self,
        self_: Resource<Node>,
    ) -> wasmtime::Result<Option<Vec<(String, Resource<Node>)>>> {
        let node = self.table.get(&self_)?;
        if let Some(children) = node.as_table() {
            let mut nodes = Vec::new();
            for (key, value) in children {
                nodes.push((key, self.table.push(value)?));
            }
            Ok(Some(nodes))
        } else {
            Ok(None)
        }
    }

    async fn get_id(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.get_id())
    }

    async fn get_kind(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.get_kind())
    }

    async fn get_name(&mut self, self_: Resource<Node>) -> wasmtime::Result<Option<String>> {
        let node = self.table.get(&self_)?;
        Ok(node.get_name())
    }

    async fn get_table(
        &mut self,
        self_: Resource<Node>,
    ) -> wasmtime::Result<Option<Vec<(String, Resource<Node>)>>> {
        let node = self.table.get(&self_)?;
        if let Some(children) = node.get_table() {
            let mut nodes = Vec::new();
            for (key, value) in children {
                nodes.push((key, self.table.push(value)?));
            }
            Ok(Some(nodes))
        } else {
            Ok(None)
        }
    }

    async fn drop(&mut self, self_: Resource<Node>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
