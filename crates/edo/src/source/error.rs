use snafu::Snafu;

/// Errors produced by the source subsystem.
///
/// Covers failures during fetching, staging, dependency resolution, vendor
/// interaction, and OCI registry operations.
#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum SourceError {
    /// A propagated environment-layer error encountered during staging.
    #[snafu(transparent)]
    Environment {
        #[snafu(source(from(crate::environment::EnvironmentError, Box::new)))]
        source: Box<crate::environment::EnvironmentError>,
    },
    /// An opaque error surfaced by a source implementation (plugin or builtin).
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    /// A propagated storage-layer error encountered during fetch or cache lookup.
    #[snafu(transparent)]
    Storage {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    /// A source's content changed after its id was memoized for this run.
    #[snafu(display("source changed during the build: expected id {expected}, fetched {actual}"))]
    Changed {
        expected: Box<crate::storage::Id>,
        actual: Box<crate::storage::Id>,
    },
    /// A node field was present but had an unexpected type.
    #[snafu(display("field '{field}' should be defined as a {type_}"))]
    Field { field: String, type_: String },
    /// An I/O error occurred during source operations.
    #[snafu(display("io eccor occured: {source}"))]
    Io { source: std::io::Error },
    /// No vendor with the given name is registered in the context.
    #[snafu(display("no vendor registered with name {name}"))]
    NoVendor { name: String },
    /// An error occurred while interacting with an OCI registry.
    #[snafu(display("error occured with oci registry: {source}"))]
    Oci { source: ocilot::error::Error },
    /// A dependency declaration is missing a version requirement.
    #[snafu(display("no version requirement provided for dependency"))]
    NoRequire,
    /// A propagated context-layer error (e.g. project/config issues).
    #[snafu(transparent)]
    Context {
        #[snafu(source(from(crate::context::ContextError, Box::new)))]
        source: Box<crate::context::ContextError>,
    },
    /// Failed to build a resolver requirement for the named dependency at the given version.
    #[snafu(display("could not build requirement for {name} at {version}"))]
    Requirement {
        name: String,
        version: semver::VersionReq,
    },
    /// The dependency resolver could not find a satisfying solution.
    #[snafu(display("resolution of vendored dependencies failed: {reason}"))]
    Resolution { reason: String },
    /// The resolved version was not found in any registered vendor's artifact store.
    #[snafu(display("could not find vended artifact for {name}@{version}"))]
    Vended {
        name: String,
        version: semver::Version,
    },
    /// The node does not represent a valid vended dependency definition.
    #[snafu(display("not a vended dependency definition"))]
    Undefined,
    /// The node does not represent a valid vendor definition.
    #[snafu(display("not a valid vendor definition"))]
    VendorUndefined,
    /// The vendor kind is not supported by any registered plugin.
    #[snafu(display("unsupported vendor kind: {kind}"))]
    Unsupported { kind: String },
}
