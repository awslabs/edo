use crate::plugin::bindings::Edo;
use crate::plugin::host::Host;
use crate::plugin::{error, Result};
use parking_lot::Mutex;
use snafu::ResultExt;
use std::sync::Arc;
use wasmtime::component::{Resource, ResourceAny};
use wasmtime::Store;

pub struct PluginHandle {
    pub store: Arc<Mutex<Store<Host>>>,
    pub handle: Arc<Edo>,
    pub me: ResourceAny,
}

unsafe impl Send for PluginHandle {}
unsafe impl Sync for PluginHandle {}

impl PluginHandle {
    pub fn new(store: Arc<Mutex<Store<Host>>>, handle: Arc<Edo>, me: ResourceAny) -> Self {
        Self { store, handle, me }
    }

    pub fn push<T>(&self, item: T) -> Result<Resource<T>>
    where
        T: Send + 'static,
    {
        self.store
            .lock()
            .data_mut()
            .table
            .push(item)
            .context(error::WasmContextSnafu)
    }

    pub fn get<T>(&self, resource: &Resource<T>) -> Result<T>
    where
        T: Send + Clone + 'static,
    {
        Ok(self
            .store
            .lock()
            .data()
            .table
            .get(resource)
            .context(error::WasmContextSnafu)?
            .clone())
    }
}
