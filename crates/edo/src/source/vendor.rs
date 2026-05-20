use super::SourceResult;
use crate::context::Node;
use arc_handle::arc_handle;
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet};

/// A remote package registry that provides versioned source artifacts.
///
/// Vendors are queried by the [`Resolver`](super::Resolver) to discover
/// available versions and transitive dependencies, and later to materialize
/// concrete [`Node`] definitions for fetching.
#[arc_handle]
#[cfg_attr(test, automock)]
#[async_trait]
pub trait Vendor {
    /// Get all versions of a given package/source name
    async fn get_options(&self, name: &str) -> SourceResult<HashSet<Version>>;
    /// Resolve a given name and version into a valid source node
    async fn resolve(&self, name: &str, version: &Version) -> SourceResult<Node>;
    /// Get all dependency requirements for a given namme and version
    async fn get_dependencies(
        &self,
        name: &str,
        version: &Version,
    ) -> SourceResult<Option<HashMap<String, VersionReq>>>;
}
