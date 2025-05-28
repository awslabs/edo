use super::handle::PluginHandle;
use crate::context::Node;
use crate::plugin::error;
use crate::source::{SourceResult, VendorImpl};
use async_trait::async_trait;
use semver::{Version, VersionReq};
use snafu::ResultExt;
use std::collections::{HashMap, HashSet};
use wasmtime::AsContextMut;

pub struct PluginVendor(PluginHandle);

impl PluginVendor {
    pub fn new(handle: PluginHandle) -> Self {
        Self(handle)
    }
}

#[async_trait]
impl VendorImpl for PluginVendor {
    async fn get_options(&self, name: &str) -> SourceResult<HashSet<Version>> {
        let self_ = &self.0;
        let this = self_.handle.edo_plugin_abi().vendor();
        let mut ctx = self_.store.lock();
        match this
            .call_get_options(ctx.as_context_mut(), self_.me, name)
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(HashSet::from_iter(
                    result.iter().map(|x| Version::parse(x).unwrap()),
                ))
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn resolve(&self, name: &str, version: &Version) -> SourceResult<Node> {
        let self_ = &self.0;
        let version = version.to_string();
        let this = self_.handle.edo_plugin_abi().vendor();
        let mut ctx = self_.store.lock();
        match this
            .call_resolve(ctx.as_context_mut(), self_.me, name, version.as_str())
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(node) => {
                drop(ctx);
                self_.get(&node).map_err(|e| e.into())
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }

    async fn get_dependencies(
        &self,
        name: &str,
        version: &Version,
    ) -> SourceResult<Option<HashMap<String, VersionReq>>> {
        let self_ = &self.0;
        let version = version.to_string();
        let this = self_.handle.edo_plugin_abi().vendor();
        let mut ctx = self_.store.lock();
        match this
            .call_get_dependencies(ctx.as_context_mut(), self_.me, name, version.as_str())
            .await
            .context(error::WasmExecSnafu)?
        {
            Ok(result) => {
                drop(ctx);
                Ok(result.map(|a| {
                    HashMap::from_iter(
                        a.iter()
                            .map(|(k, v)| (k.to_string(), VersionReq::parse(v).unwrap())),
                    )
                }))
            }
            Err(e) => {
                drop(ctx);
                let guest = self_.get(&e)?;
                error::GuestSnafu { guest }.fail().map_err(|e| e.into())
            }
        }
    }
}
