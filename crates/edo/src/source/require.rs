use super::error;
use semver::VersionReq;

use crate::context::{Addr, Context, Requirement};

/// A resolved dependency requirement parsed from an `edo.toml`
/// `[requires.<addr>]` block.
///
/// Holds enough information for the [`super::Resolver`] to feed the
/// dependency into resolvo: the package name (taken from the address),
/// the kind of source (used to pick a vendor when none is named),
/// the semver constraint, and the originating address.
#[derive(Clone, Hash, PartialEq, Eq)]
pub struct Dependency {
    /// The address of the requires block.
    pub addr: Addr,
    /// The source kind (e.g. `"image"`, `"git"`).
    pub kind: String,
    /// The package name being depended upon (the address's leaf segment).
    pub name: String,
    /// The semver requirement that must be satisfied.
    pub version: VersionReq,
    /// Optional vendor name constraining which registry to resolve from.
    pub vendor: Option<String>,
}

impl Dependency {
    /// Builds a [`Dependency`] from the address of a requires block and its
    /// typed [`Requirement`].
    ///
    /// `_ctx` is currently unused but kept in the signature so callers can
    /// pass the build context they hold; future versions may consult it
    /// for default-vendor lookup.
    pub async fn new(
        addr: &Addr,
        requirement: &Requirement,
        _ctx: &Context,
    ) -> std::result::Result<Self, super::error::SourceError> {
        // The dependency's package name comes from the address's leaf
        // segment. If the address ends with a trailing empty segment we
        // fall back to the full id, which keeps the error path producing
        // a usable string for `Resolver::build_db`.
        let name = addr
            .to_id()
            .rsplit('/')
            .next()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| addr.to_id());
        if name.is_empty() {
            return error::UndefinedSnafu {}.fail();
        }
        Ok(Self {
            addr: addr.clone(),
            kind: requirement.kind.clone(),
            name,
            version: requirement.at.clone(),
            vendor: None,
        })
    }
}
