use std::{collections::HashMap, path::Path};

use super::{error, Addr, ContextResult, Log, LogManager};
use crate::{
    environment::{Environment, Farm},
    storage::Storage,
    transform::Transform,
};
use snafu::OptionExt;

/// A handle is passed to transforms where it needs to look up
/// things in the transform state.
#[derive(Clone)]
pub struct Handle {
    pub log: LogManager,
    pub storage: Storage,
    pub transforms: HashMap<Addr, Transform>,
    pub farms: HashMap<Addr, Farm>,
    pub args: HashMap<String, String>,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub fn log(&self) -> &LogManager {
        &self.log
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn get(&self, addr: &Addr) -> Option<Transform> {
        self.transforms.get(addr).cloned()
    }

    pub fn transforms(&self) -> &HashMap<Addr, Transform> {
        &self.transforms
    }

    pub fn args(&self) -> &HashMap<String, String> {
        &self.args
    }

    pub async fn create_environment(
        &self,
        log: &Log,
        addr: &Addr,
        path: &Path,
    ) -> ContextResult<Environment> {
        let farm = self
            .farms
            .get(addr)
            .context(error::NoEnvironmentFoundSnafu { addr: addr.clone() })?;
        let env = farm.create(log, path).await?;
        Ok(env)
    }
}
