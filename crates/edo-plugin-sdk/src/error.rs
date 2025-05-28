use crate::bindings::edo::plugin::host;
use snafu::Snafu;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("{kind} definitions require a field '{field}' with type '{type_}'"))]
    Field {
        kind: String,
        field: String,
        type_: String,
    },
    #[snafu(display("{}", host.to_string()))]
    Host { host: host::Error },
    #[snafu(display("invalid configuration as node was not a definition"))]
    NotDefinition,
    #[snafu(display("could not find dependency {addr} in project"))]
    NotFound { addr: String },
    #[snafu(display("this plugin does not implement this functionality"))]
    NotImplemented,
}

impl From<host::Error> for Error {
    fn from(value: host::Error) -> Self {
        Self::Host { host: value }
    }
}

impl Into<host::Error> for Error {
    fn into(self) -> host::Error {
        match self {
            Self::Host { host } => host,
            ref value => host::Error::new("bottlerocket", &value.to_string()),
        }
    }
}
