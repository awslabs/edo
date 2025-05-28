use super::error;
use crate::context::Log;

use super::error::wasm_ok;
use super::WasmResult;
use super::{bindings::edo::plugin::host, host::Host};
use snafu::ResultExt;
use std::io::Write;
use wasmtime::component::Resource;

impl host::HostLog for Host {
    async fn write(&mut self, self_: Resource<Log>, message: Vec<u8>) -> WasmResult<u64> {
        let file = self.table.get_mut(&self_)?;
        let result = wasm_ok!(with self.table => file.write(message.as_slice()).context(error::IoSnafu).map(|x| x as u64));
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Log>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
