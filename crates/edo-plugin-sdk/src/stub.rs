use super::bindings::edo::plugin::host;
use super::bindings::exports::edo::plugin::abi;
use super::bindings::exports::edo::plugin::abi::{
    GuestBackend, GuestEnvironment, GuestFarm, GuestSource, GuestVendor,
};
use super::{HostResult, error};

pub struct Stub;

impl GuestFarm for Stub {
    fn setup(&self, _: &host::Log, _: &host::Storage) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn create(&self, _: &host::Log, _: String) -> HostResult<abi::Environment> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }
}

impl GuestEnvironment for Stub {
    fn expand(&self, _: String) -> HostResult<String> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn create_dir(&self, _: String) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn set_env(&self, _: String, _: String) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn get_env(&self, _: String) -> Option<String> {
        None
    }

    fn setup(&self, _: &host::Log, _: &host::Storage) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn up(&self, _: &host::Log) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn down(&self, _: &host::Log) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn clean(&self, _: &host::Log) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn write(&self, _: String, _: &host::Reader) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn unpack(&self, _: String, _: &host::Reader) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn read(&self, _: String, _: &host::Writer) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn cmd(&self, _: &host::Log, _: &host::Id, _: String, _: String) -> HostResult<bool> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn run(&self, _: &host::Log, _: &host::Id, _: String, _: &host::Command) -> HostResult<bool> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn shell(&self, _: String) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }
}

impl GuestSource for Stub {
    fn get_unique_id(&self) -> HostResult<host::Id> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn fetch(&self, _: &host::Log, _: &host::Storage) -> HostResult<host::Artifact> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn stage(
        &self,
        _: &host::Log,
        _: &host::Storage,
        _: &host::Environment,
        _: String,
    ) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }
}

impl GuestVendor for Stub {
    fn get_options(&self, _: String) -> HostResult<Vec<String>> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn resolve(&self, _: String, _: String) -> HostResult<host::Node> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn get_dependencies(&self, _: String, _: String) -> HostResult<Option<Vec<(String, String)>>> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }
}

impl GuestBackend for Stub {
    fn ls(&self) -> HostResult<Vec<host::Id>> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn has(&self, _: &host::Id) -> HostResult<bool> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn open(&self, _: &host::Id) -> HostResult<host::Artifact> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn save(&self, _: &host::Artifact) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn del(&self, _: &host::Id) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn copy(&self, _: &host::Id, _: &host::Id) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn prune(&self, _: &host::Id) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn prune_all(&self) -> HostResult<()> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn read(&self, _: &host::Layer) -> HostResult<host::Reader> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn start_layer(&self) -> HostResult<host::Writer> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }

    fn finish_layer(
        &self,
        _: String,
        _: Option<String>,
        writer: &host::Writer,
    ) -> HostResult<host::Layer> {
        error::NotImplementedSnafu {}.fail().map_err(|e| e.into())
    }
}
