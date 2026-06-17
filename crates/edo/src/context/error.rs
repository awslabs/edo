//! Error types for the context module.
//!
//! All fallible operations in the context module return
//! [`ContextError`] via the [`ContextResult`](super::ContextResult) alias.
//! Variants use SNAFU's `#[snafu(display(...))]` attribute for formatting.

use snafu::Snafu;
use tracing_subscriber::util::TryInitError;

use super::Addr;

/// Enumerates all errors that can occur within the context module.
#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum ContextError {
    /// A required field was missing or had the wrong type.
    #[snafu(display("expected a field named '{field}' with a type of {type_}"))]
    Field {
        /// Name of the expected field.
        field: String,
        /// Expected type description.
        type_: String,
    },
    /// The user's home directory could not be determined.
    #[snafu(display("failed to find home directory"))]
    Home,
    #[snafu(display("invalid element for {kind} at {addr}: {source}"))]
    Invalid {
        kind: String,
        addr: Addr,
        source: serde_json::Error,
    },
    /// An I/O operation failed.
    #[snafu(display("io error occured: {source}"))]
    Io {
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Dependencies changed since the lockfile was generated.
    #[snafu(display("dependencies have changed, run edo update to update the lockfile"))]
    DependencyChange,
    /// TOML deserialization failed.
    #[snafu(display("failed to deserialize toml: {source}"))]
    Deserialize {
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// Logging subsystem initialization failed.
    #[snafu(display("failed to initialize logging: {source}"))]
    Log {
        /// The underlying tracing initialization error.
        source: TryInitError,
    },
    /// The lockfile is missing resolution data for an address.
    #[snafu(display("lockfile is missing resolution data for: {addr}"))]
    MalformedLock {
        /// The address that has no lock entry.
        addr: Addr,
    },
    /// A configuration value could not be read as a node.
    #[snafu(display("could not read to a configuration node"))]
    Node,
    /// A node is missing one or more required keys.
    #[snafu(display("node is missing required keys {}", keys.join(", ")))]
    NodeMissingKeys {
        /// The list of missing key names.
        keys: Vec<String>,
    },
    /// A node is missing a kind definition.
    #[snafu(display("node is missing a kind definition"))]
    NodeNoKind,
    /// A node is missing a name.
    #[snafu(display("node is missing a name"))]
    NodeNoName,
    /// A node is missing an id.
    #[snafu(display("node is missing an id"))]
    NodeNoId,
    /// The block id could not be determined.
    #[snafu(display("could not determine block id"))]
    NoBlockId,
    /// The block is not an environment definition.
    #[snafu(display("block is not an environment definition"))]
    NotEnvironment,
    /// No environment was found for the given address.
    #[snafu(display("no environment found with addr '{addr}'"))]
    NoEnvironmentFound {
        /// The address that was looked up.
        addr: Addr,
    },
    /// No plugin is loaded for the given address.
    #[snafu(display("no plugin loaded with addr '{addr}'"))]
    NoPlugin {
        /// The plugin address that was not found.
        addr: Addr,
    },
    /// No loaded plugin supports the requested component kind.
    #[snafu(display("no implementation is loaded that supports a {component} of kind {kind}"))]
    NoProvider {
        /// The component type being requested.
        component: String,
        /// The kind discriminator that no plugin supports.
        kind: String,
    },
    /// The block is not a transform definition.
    #[snafu(display("block is not a transform definition"))]
    NotTransform,
    /// The id is not valid for a source definition.
    #[snafu(display("'{id}' is not a valid block id for a source definition"))]
    NotValidSource {
        /// The invalid source id.
        id: String,
    },
    /// A `source = "..."` reference points to an address that has no
    /// matching `[source.<addr>]` entry in the merged schema.
    #[snafu(display(
        "source reference '{id}' is not defined; add a [source.\"{id}\"] entry or fix the reference"
    ))]
    MissingSource {
        /// The unresolved source address.
        id: String,
    },
    /// The resolver returned a vendor name that is not registered. This
    /// usually indicates a mismatch between `[requires]` and `[vendor]`
    /// configuration.
    #[snafu(display(
        "resolver picked vendor '{name}' which is not registered; add a [vendor.\"{name}\"] entry or fix the requires reference"
    ))]
    MissingVendor {
        /// The vendor name reported by the resolver.
        name: String,
    },
    /// The dependency resolver task panicked or was cancelled.
    #[snafu(display("dependency resolver task failed: {source}"))]
    ResolverJoin {
        /// The underlying tokio join error.
        source: tokio::task::JoinError,
    },
    /// The block is not a vendor definition.
    #[snafu(display("block is not a vendor definition"))]
    NotVendor,
    /// A component subsystem error.
    #[snafu(transparent)]
    Component {
        /// The underlying plugin error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// An environment subsystem error.
    #[snafu(transparent)]
    Environment {
        /// The underlying environment error.
        source: crate::environment::EnvironmentError,
    },
    /// A scheduler subsystem error.
    #[snafu(transparent)]
    Scheduler {
        /// The underlying scheduler error.
        #[snafu(source(from(crate::scheduler::error::SchedulerError, Box::new)))]
        source: Box<crate::scheduler::error::SchedulerError>,
    },
    /// JSON serialization failed.
    #[snafu(display("failed to serialize to json: {source}"))]
    Serialize {
        /// The underlying JSON serialization error.
        source: serde_json::Error,
    },
    #[snafu(display(
        "invalid `source` attribute to element, it must be a single string, list of strings or a map of string to string[]"
    ))]
    SourceMap,
    /// A storage subsystem error.
    #[snafu(transparent)]
    Storage {
        /// The underlying storage error.
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    /// A transform subsystem error.
    #[snafu(transparent)]
    Transform {
        /// The underlying transform error.
        source: crate::transform::TransformError,
    },
    /// A source subsystem error.
    #[snafu(transparent)]
    Source {
        /// The underlying source error.
        source: crate::source::SourceError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_field() {
        let e = ContextError::Field {
            field: "kind".into(),
            type_: "string".into(),
        };
        assert_eq!(
            e.to_string(),
            "expected a field named 'kind' with a type of string"
        );
    }

    #[test]
    fn display_home() {
        let e = ContextError::Home;
        assert_eq!(e.to_string(), "failed to find home directory");
    }

    #[test]
    fn display_dependency_change() {
        let e = ContextError::DependencyChange;
        assert_eq!(
            e.to_string(),
            "dependencies have changed, run edo update to update the lockfile"
        );
    }

    #[test]
    fn display_malformed_lock() {
        let addr = Addr::parse("//x/y").unwrap();
        let e = ContextError::MalformedLock { addr };
        assert_eq!(
            e.to_string(),
            "lockfile is missing resolution data for: //x/y"
        );
    }

    #[test]
    fn display_node() {
        let e = ContextError::Node;
        assert_eq!(e.to_string(), "could not read to a configuration node");
    }

    #[test]
    fn display_node_missing_keys() {
        let e = ContextError::NodeMissingKeys {
            keys: vec!["foo".into(), "bar".into()],
        };
        assert_eq!(e.to_string(), "node is missing required keys foo, bar");
    }

    #[test]
    fn display_node_no_kind() {
        let e = ContextError::NodeNoKind;
        assert_eq!(e.to_string(), "node is missing a kind definition");
    }

    #[test]
    fn display_node_no_name() {
        let e = ContextError::NodeNoName;
        assert_eq!(e.to_string(), "node is missing a name");
    }

    #[test]
    fn display_node_no_id() {
        let e = ContextError::NodeNoId;
        assert_eq!(e.to_string(), "node is missing an id");
    }

    #[test]
    fn display_no_block_id() {
        let e = ContextError::NoBlockId;
        assert_eq!(e.to_string(), "could not determine block id");
    }

    #[test]
    fn display_not_environment() {
        let e = ContextError::NotEnvironment;
        assert_eq!(e.to_string(), "block is not an environment definition");
    }

    #[test]
    fn display_no_environment_found() {
        let addr = Addr::parse("//x/y").unwrap();
        let e = ContextError::NoEnvironmentFound { addr };
        assert_eq!(e.to_string(), "no environment found with addr '//x/y'");
    }

    #[test]
    fn display_no_plugin() {
        let addr = Addr::parse("//x/y").unwrap();
        let e = ContextError::NoPlugin { addr };
        assert_eq!(e.to_string(), "no plugin loaded with addr '//x/y'");
    }

    #[test]
    fn display_no_provider() {
        let e = ContextError::NoProvider {
            component: "storage".into(),
            kind: "s3".into(),
        };
        assert_eq!(
            e.to_string(),
            "no implementation is loaded that supports a storage of kind s3"
        );
    }

    #[test]
    fn display_not_transform() {
        let e = ContextError::NotTransform;
        assert_eq!(e.to_string(), "block is not a transform definition");
    }

    #[test]
    fn display_not_valid_source() {
        let e = ContextError::NotValidSource {
            id: "bad-id".into(),
        };
        assert_eq!(
            e.to_string(),
            "'bad-id' is not a valid block id for a source definition"
        );
    }

    #[test]
    fn display_not_vendor() {
        let e = ContextError::NotVendor;
        assert_eq!(e.to_string(), "block is not a vendor definition");
    }
}
