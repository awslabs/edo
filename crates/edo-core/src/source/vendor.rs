use super::SourceResult;
use crate::context::Node;
use crate::def_trait;
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet};

def_trait! {
    "Defines the interface that all source vendors must implement" =>
    "A Vendor represents a provider for sources with support for dependency resolution" =>
    Vendor: VendorImpl {
        "Get all versions of a given package/source name" =>
        get_options(name: &str) -> SourceResult<HashSet<Version>>;
        "Resolve a given name and version into a valid source node" =>
        resolve(name: &str, version: &Version) -> SourceResult<Node>;
        "Get all dependency requirements for a given name and version" =>
        get_dependencies(name: &str, version: &Version) -> SourceResult<Option<HashMap<String, VersionReq>>>
    }
}
