use snafu::Snafu;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum SourceError {
    #[snafu(transparent)]
    Environment {
        #[snafu(source(from(crate::environment::EnvironmentError, Box::new)))]
        source: Box<crate::environment::EnvironmentError>,
    },
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(transparent)]
    Plugin {
        #[snafu(source(from(crate::plugin::error::PluginError, Box::new)))]
        source: Box<crate::plugin::error::PluginError>,
    },
    #[snafu(transparent)]
    Storage {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    #[snafu(display("field '{field}' should be defined as a {type_}"))]
    Field { field: String, type_: String },
    #[snafu(display("io eccor occured: {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("no vendor registered with name {name}"))]
    NoVendor { name: String },
    #[snafu(display("error occured with oci registry: {source}"))]
    Oci { source: ocilot::error::Error },
    #[snafu(display("no version requirement provided for dependency"))]
    NoRequire,
    #[snafu(transparent)]
    Context {
        #[snafu(source(from(crate::context::ContextError, Box::new)))]
        source: Box<crate::context::ContextError>,
    },
    #[snafu(display("could not build requirement for {name} at {version}"))]
    Requirement {
        name: String,
        version: semver::VersionReq,
    },
    #[snafu(display("resolution of vendored dependencies failed: {reason}"))]
    Resolution { reason: String },
    #[snafu(display("could not find vended artifact for {name}@{version}"))]
    Vended {
        name: String,
        version: semver::Version,
    },
    #[snafu(display("not a vended dependency definition"))]
    Undefined,
    #[snafu(display("not a valid vendor definition"))]
    VendorUndefined,
    #[snafu(display("unsupported vendor kind: {kind}"))]
    Unsupported { kind: String },
}
