use super::EnvResult;
use super::Environment;
use crate::context::Log;
use crate::def_trait;
use crate::storage::Storage;
use std::path::Path;

def_trait! {
    "Defines the interface that implementations of an environment farm must support" =>
    "An Environment farm determines how to create new environments for transforms" =>
    Farm: FarmImpl {
        "Setup can be used for any one time initializations required for an environment farm" =>
        setup(log: &Log, storage: &Storage) -> EnvResult<()>;
        "Create a new environment using this farm" =>
        create(log: &Log, path: &Path) -> EnvResult<Environment>
    }
}
