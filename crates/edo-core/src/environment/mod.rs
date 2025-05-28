use crate::def_trait;

use super::storage::Id;
use crate::context::Log;
use crate::util::{Reader, Writer};
use std::path::{Path, PathBuf};

use super::storage::Storage;

mod command;
pub mod error;
mod farm;

pub use command::*;
pub use error::EnvironmentError;
pub use farm::*;

pub type EnvResult<T> = std::result::Result<T, error::EnvironmentError>;

def_trait! {
    "Defines the interface that implementations of an environment must support" =>
    "An Environment represents where a transform is executed and generally outside of local environments provide some level of sandboxing" =>
    Environment: EnvironmentImpl {
        "Expand the provided path to a root absolute path inside of the environment" =>
        expand(path: &Path) -> EnvResult<PathBuf>;
        "Create a directory inside the environment" =>
        create_dir(path: &Path) -> EnvResult<()>;
        "Set environment variable" =>
        set_env(key: &str, value: &str) -> EnvResult<()>;
        "Get an environment variable" =>
        get_env(key: &str) -> Option<String>;
        "Setup the environment for execution" =>
        setup(log: &Log, storage: &Storage) -> EnvResult<()>;
        "Spin the environment up" =>
        up(log: &Log) -> EnvResult<()>;
        "Spin the environment down" =>
        down(log: &Log) -> EnvResult<()>;
        "Clean the environment" =>
        clean(log: &Log) -> EnvResult<()>;
        "Write a file into the environment from a given reader" =>
        write(path: &Path, reader: Reader) -> EnvResult<()>;
        "Unpack an archive into the environment from a given reader" =>
        unpack(path: &Path, reader: Reader) -> EnvResult<()>;
        "Read or archive a path in the environment to a given writer" =>
        read(path: &Path, writer: Writer) -> EnvResult<()>;
        "Run a single command in the environment" =>
        cmd(log: &Log, id: &Id, path: &Path, command: &str) -> EnvResult<bool>;
        "Run a deferred command in the environment" =>
        run(log: &Log, id: &Id, path: &Path, command: &Command) -> EnvResult<bool>
        : "Open a shell in the environment" =>
        shell(path: &Path) -> EnvResult<()>
    }
}

impl Environment {
    pub fn defer_cmd(&self, log: &Log, id: &Id) -> Command {
        Command::new(log, id, self)
    }
}
