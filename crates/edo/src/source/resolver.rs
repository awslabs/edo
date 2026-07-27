use dashmap::DashMap;
use resolvo::utils::Pool;
use resolvo::{
    Candidates, ConditionalRequirement, Dependencies, DependencyProvider, Interner,
    KnownDependencies, NameId, Problem, Requirement, SolvableId, Solver, StringId,
    UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
};
use semver::Version;
use std::fmt;
use std::sync::Arc;
use tokio::runtime::Handle;

use crate::context::Addr;

use super::require::Dependency;
use super::version::EdoVersion;
use super::version::EdoVersionSet;
use super::{SourceResult as Result, Vendor, error};
use std::collections::{HashMap, HashSet};

/// Semver-based dependency resolver backed by [`resolvo`].
///
/// Maintains a pool of interned packages and version sets populated by
/// registered [`Vendor`]s. Call [`Resolver::build_db`] for each package name
/// to populate candidates, then [`Resolver::resolve`] to compute a satisfying
/// assignment.
#[derive(Clone, Default)]
pub struct Resolver {
    pool: Arc<Pool<EdoVersionSet>>,
    name_to_vs: DashMap<NameId, Set>,
    vendors: DashMap<String, Vendor>,
}

#[derive(Clone)]
enum Set {
    Single(VersionSetId),
    Union(VersionSetUnionId),
}

unsafe impl Send for Resolver {}
unsafe impl Sync for Resolver {}

impl Resolver {
    /// Resolve a set of dependency requirements into concrete (vendor, name, version) triples.
    ///
    /// Returns a map from each dependency's address to its resolved vendor name,
    /// package name, and version.
    pub fn resolve(
        &self,
        requires: Vec<Dependency>,
    ) -> Result<HashMap<Addr, (String, String, Version)>> {
        let handle = Handle::current();
        let mut targets: HashMap<(String, Option<String>), HashSet<Addr>> = HashMap::new();
        let mut solver = Solver::new(self.clone()).with_runtime(handle);
        let mut requirements = Vec::new();
        for entry in requires.iter() {
            targets
                .entry((entry.name.clone(), entry.vendor.clone()))
                .or_default()
                .insert(entry.addr.clone());
            let requirement = self.build_requirement(entry)?;
            requirements.push(ConditionalRequirement {
                condition: None,
                requirement,
            });
        }
        let problem = Problem::new().requirements(requirements);
        let resolution = match solver.solve(problem) {
            Ok(result) => Ok(result),
            Err(UnsolvableOrCancelled::Unsolvable(conflict)) => error::ResolutionSnafu {
                reason: conflict.display_user_friendly(&solver).to_string(),
            }
            .fail(),
            Err(UnsolvableOrCancelled::Cancelled(_)) => error::ResolutionSnafu {
                reason: "resolution was cancelled",
            }
            .fail(),
        }?;
        let mut found = HashMap::new();
        for s_id in resolution.iter() {
            let solvable = self.pool.resolve_solvable(*s_id);
            let name = self.pool.resolve_package_name(solvable.name);
            let vendor = self.vendors.get(&solvable.record.vendor()).unwrap();
            if let Some(addr) = targets.get(&(name.clone(), Some(vendor.key().clone()))) {
                for entry in addr {
                    found.insert(
                        entry.clone(),
                        (
                            vendor.key().clone(),
                            name.clone(),
                            solvable.record.version(),
                        ),
                    );
                }
            }
            if let Some(addr) = targets.get(&(name.clone(), None)) {
                for entry in addr {
                    found.insert(
                        entry.clone(),
                        (
                            vendor.key().clone(),
                            name.clone(),
                            solvable.record.version(),
                        ),
                    );
                }
            }
        }
        Ok(found)
    }

    /// Populate the resolver's internal database for `name` by querying all registered vendors.
    ///
    /// Must be called for every package name that appears in a dependency graph
    /// before calling [`Resolver::resolve`].
    pub async fn build_db(&self, name: &str) -> Result<()> {
        // Snapshot the vendor list to a plain `Vec` so we don't hold a
        // DashMap iterator guard across `.await` (get_options can block
        // on network) or across the `name_to_vs` writes below (which
        // would risk a same-shard read+write deadlock inside DashMap).
        let vendors: Vec<(String, Vendor)> = self
            .vendors
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        for (vendor_name, vendor) in vendors {
            let version_set = vendor.get_options(name).await?;
            let name_id = self.pool.intern_package_name(name.to_string());
            let mut edo_versions = Vec::new();
            for version in version_set {
                let edo_version = EdoVersion::new(&vendor_name, &version);
                self.pool.intern_solvable(name_id, edo_version.clone());
                edo_versions.push(edo_version.clone());
            }
            let vsid = self
                .pool
                .intern_version_set(name_id, EdoVersionSet::new(edo_versions.as_slice()));
            // Compute the next `Set` value with the read guard released
            // before writing back: DashMap uses a sharded RwLock, and
            // holding a read guard on a shard while calling `insert`
            // that lands on the same shard will deadlock.
            let existing = self.name_to_vs.get(&name_id).map(|e| e.value().clone());
            let next = match existing {
                Some(Set::Union(union_id)) => {
                    let vs_union = self.pool.resolve_version_set_union(union_id);
                    Set::Union(self.pool.intern_version_set_union(vsid, vs_union))
                }
                Some(Set::Single(vs_id)) => Set::Union(
                    self.pool
                        .intern_version_set_union(vsid, [vs_id].iter().cloned()),
                ),
                None => Set::Single(vsid),
            };
            self.name_to_vs.insert(name_id, next);
        }
        Ok(())
    }

    /// Register a vendor under the given name for use during resolution.
    pub fn add_vendor(&mut self, name: &str, vendor: Vendor) {
        self.vendors.insert(name.to_string(), vendor);
    }

    /// Build a [`Requirement`] from a [`Dependency`] node against the current pool state.
    pub fn build_requirement(&self, node: &Dependency) -> Result<Requirement> {
        let dep_id = if let Some(name_id) = self.pool.lookup_package_name(&node.name) {
            name_id
        } else {
            return error::RequirementSnafu {
                name: node.name.clone(),
                version: node.version.clone(),
            }
            .fail();
        };
        let mut matches = Vec::new();
        let require = node.version.clone();
        if let Some(entry) = self.name_to_vs.get(&dep_id) {
            match entry.value() {
                Set::Union(union_id) => {
                    let union = self.pool.resolve_version_set_union(*union_id);
                    for vs_id in union {
                        let version_set = self.pool.resolve_version_set(vs_id);
                        for version in version_set.get() {
                            let mut flag = version.matches(&require);
                            if let Some(vendor) = node.vendor.as_ref() {
                                flag &= *vendor == version.vendor();
                            }
                            if flag {
                                matches.push(version.clone());
                            }
                        }
                    }
                }
                Set::Single(vs_id) => {
                    let version_set = self.pool.resolve_version_set(*vs_id);
                    for version in version_set.get() {
                        let mut flag = version.matches(&require);
                        if let Some(vendor) = node.vendor.as_ref() {
                            flag &= *vendor == version.vendor();
                        }
                        if flag {
                            matches.push(version.clone());
                        }
                    }
                }
            }
        }
        if !matches.is_empty() {
            let vs_id = self
                .pool
                .intern_version_set(dep_id, EdoVersionSet::new(matches.as_slice()));
            Ok(Requirement::Single(vs_id))
        } else {
            error::RequirementSnafu {
                name: node.name.clone(),
                version: node.version.clone(),
            }
            .fail()
        }
    }
}

impl Interner for Resolver {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl fmt::Display + '_ {
        let solvable = self.pool.resolve_solvable(solvable);
        format!(
            "{}@{}",
            self.pool.resolve_package_name(solvable.name),
            solvable.record
        )
    }

    fn display_name(&self, name: NameId) -> impl fmt::Display + '_ {
        self.pool.resolve_package_name(name)
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl fmt::Display + '_ {
        self.pool.resolve_version_set(version_set)
    }

    fn display_string(&self, string_id: StringId) -> impl fmt::Display + '_ {
        self.pool.resolve_string(string_id)
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }

    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(version_set_union)
    }

    fn resolve_condition(&self, condition: resolvo::ConditionId) -> resolvo::Condition {
        self.pool.resolve_condition(condition).clone()
    }
}

impl DependencyProvider for Resolver {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let set = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .filter(|x| {
                let solvable = self.pool.resolve_solvable(*(*x));
                let flag = set.get().contains(&solvable.record);
                if inverse { !flag } else { flag }
            })
            .cloned()
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<resolvo::Candidates> {
        match self.name_to_vs.get(&name) {
            Some(entry) => match entry.value() {
                Set::Union(union_id) => {
                    let mut candidates = Candidates::default();
                    let vs_ids = self.pool.resolve_version_set_union(*union_id);
                    for vs_id in vs_ids {
                        let vs = self.pool.resolve_version_set(vs_id);
                        for entry in vs.get() {
                            let sid = self.pool.intern_solvable(name, entry.clone());
                            candidates.candidates.push(sid);
                        }
                    }
                    Some(candidates)
                }
                Set::Single(vs_id) => {
                    let mut candidates = Candidates::default();
                    let set = self.pool.resolve_version_set(*vs_id);
                    for entry in set.get() {
                        let sid = self.pool.intern_solvable(name, entry.clone());
                        candidates.candidates.push(sid);
                    }
                    if candidates.candidates.len() == 1 {
                        candidates.locked = candidates.candidates.first().cloned();
                    }
                    Some(candidates)
                }
            },
            None => None,
        }
    }

    async fn sort_candidates(
        &self,
        _solver: &resolvo::SolverCache<Self>,
        solvables: &mut [SolvableId],
    ) {
        // resolvo iterates candidates from front to back and prefers the
        // first satisfying assignment, so put the highest version first.
        // Sorting ascending here would make the solver pick the lowest
        // matching version for constraints like `>=14` (`14.0.0`) instead
        // of the newest available (`14.9.0`).
        solvables.sort_by(|x, y| {
            let left = self.pool.resolve_solvable(*x);
            let right = self.pool.resolve_solvable(*y);
            right.record.version().cmp(&left.record.version())
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> resolvo::Dependencies {
        let solvable = self.pool.resolve_solvable(solvable);
        let name = self.pool.resolve_package_name(solvable.name);
        let version = solvable.record.clone();
        let mut dependencies = Dependencies::Known(KnownDependencies::default());
        let vendor = self.vendors.get(&version.vendor()).unwrap();
        if let Some(found) = vendor
            .get_dependencies(name, &version.version())
            .await
            .ok()
            .flatten()
        {
            let mut known = KnownDependencies::default();
            for (name, version_req) in found.iter() {
                let dep_id = if let Some(name_id) = self.pool.lookup_package_name(name) {
                    name_id
                } else {
                    return Dependencies::Unknown(self.pool.intern_string(format!("could not find dependency with name {} and version requirement {} in any registered vendor", name, version_req)));
                };
                let mut matches = Vec::new();
                if let Some(entry) = self.name_to_vs.get(&dep_id) {
                    match entry.value() {
                        Set::Union(union_id) => {
                            let union = self.pool.resolve_version_set_union(*union_id);
                            for vs_id in union {
                                let version_set = self.pool.resolve_version_set(vs_id);
                                for version in version_set.get() {
                                    if version.matches(version_req) {
                                        matches.push(version.clone());
                                    }
                                }
                            }
                        }
                        Set::Single(vs_id) => {
                            let version_set = self.pool.resolve_version_set(*vs_id);
                            for version in version_set.get() {
                                if version.matches(version_req) {
                                    matches.push(version.clone());
                                }
                            }
                        }
                    }
                }
                if !matches.is_empty() {
                    let vs_id = self
                        .pool
                        .intern_version_set(dep_id, EdoVersionSet::new(matches.as_slice()));
                    known.requirements.push(ConditionalRequirement {
                        condition: None,
                        requirement: Requirement::Single(vs_id),
                    });
                }
            }
            dependencies = Dependencies::Known(known);
        }

        dependencies
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Vendor, VendorImpl};
    use super::*;
    use crate::context::{Addr, Element};
    use crate::source::SourceResult;
    use async_trait::async_trait;
    use semver::{Version, VersionReq};
    use std::collections::{HashMap, HashSet};

    /// A hand-rolled `VendorImpl` for resolver tests: exposes a fixed set
    /// of versions and a fixed dependency map without touching the network.
    ///
    /// Mirrors the shape of the mock in `super::tests` — we can't use
    /// mockall here because `Vendor` is an `arc_handle` trait and mocks
    /// don't cross the `arc_handle` boundary cleanly.
    struct FakeVendor {
        versions: HashSet<Version>,
        deps: HashMap<Version, HashMap<String, VersionReq>>,
    }

    #[async_trait]
    impl VendorImpl for FakeVendor {
        async fn get_options(&self, _name: &str) -> SourceResult<HashSet<Version>> {
            Ok(self.versions.clone())
        }

        async fn resolve(&self, name: &str, _version: &Version) -> SourceResult<Element> {
            Ok(Element::builder()
                .addr(Addr::parse(name)?)
                .kind("test")
                .config(std::collections::BTreeMap::new())
                .build())
        }

        async fn get_dependencies(
            &self,
            _name: &str,
            version: &Version,
        ) -> SourceResult<Option<HashMap<String, VersionReq>>> {
            Ok(self.deps.get(version).cloned())
        }
    }

    fn ver(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn req(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    /// `>=14` must resolve to the highest available version (14.9.0),
    /// not the lowest (14.0.0). `sort_candidates` orders solvables
    /// descending so resolvo picks the newest satisfying version first.
    #[tokio::test]
    async fn resolves_to_highest_matching_version() {
        let vendor = Vendor::new(FakeVendor {
            versions: HashSet::from_iter([
                ver("14.0.0"),
                ver("14.5.0"),
                ver("14.9.0"),
                ver("13.0.0"),
                ver("15.0.0"),
            ]),
            deps: HashMap::new(),
        });
        let mut resolver = Resolver::default();
        resolver.add_vendor("//vendor/test", vendor);
        resolver.build_db("kit").await.expect("build_db");

        let addr = Addr::parse("//kits/kit").unwrap();
        let dep = Dependency {
            addr: addr.clone(),
            kind: "kit-image".to_string(),
            name: "kit".to_string(),
            version: req(">=14"),
            vendor: Some("//vendor/test".to_string()),
        };
        let result = tokio::task::spawn_blocking(move || resolver.resolve(vec![dep]))
            .await
            .expect("join")
            .expect("resolve");
        let (_, _, v) = result.get(&addr).expect("found addr");
        assert_eq!(*v, ver("15.0.0"), "resolver must prefer highest version");
    }

    /// When a kit's transitive SDK requirement (surfaced via
    /// `get_dependencies`) conflicts with the project's SDK requirement,
    /// resolution must fail rather than silently drop the constraint or
    /// pick a version that violates it.
    #[tokio::test]
    async fn conflicting_transitive_sdk_requirement_fails_resolution() {
        // Kit vendor: two versions, both pinning SDK to 0.76.0.
        let kit_vendor = Vendor::new(FakeVendor {
            versions: HashSet::from_iter([ver("14.0.0"), ver("14.9.0")]),
            deps: HashMap::from([
                (
                    ver("14.0.0"),
                    HashMap::from([("sdk".to_string(), req("=0.72.0"))]),
                ),
                (
                    ver("14.9.0"),
                    HashMap::from([("sdk".to_string(), req("=0.76.0"))]),
                ),
            ]),
        });
        // SDK vendor: publishes 0.72.0, 0.76.0, and 0.77.0.
        let sdk_vendor = Vendor::new(FakeVendor {
            versions: HashSet::from_iter([ver("0.72.0"), ver("0.76.0"), ver("0.77.0")]),
            deps: HashMap::new(),
        });
        let mut resolver = Resolver::default();
        resolver.add_vendor("//vendor/kits", kit_vendor);
        resolver.add_vendor("//vendor/sdk", sdk_vendor);
        resolver.build_db("kit").await.expect("build_db kit");
        resolver.build_db("sdk").await.expect("build_db sdk");

        let kit_addr = Addr::parse("//kits/kit").unwrap();
        let sdk_addr = Addr::parse("//sdk").unwrap();
        let requires = vec![
            Dependency {
                addr: kit_addr,
                kind: "kit-image".to_string(),
                name: "kit".to_string(),
                version: req(">=14"),
                vendor: Some("//vendor/kits".to_string()),
            },
            Dependency {
                addr: sdk_addr,
                kind: "image".to_string(),
                name: "sdk".to_string(),
                version: req("=0.77.0"),
                vendor: Some("//vendor/sdk".to_string()),
            },
        ];
        let err = tokio::task::spawn_blocking(move || resolver.resolve(requires))
            .await
            .expect("join")
            .expect_err("expected resolution to fail");
        // Error variant is `Resolution { reason: .. }`; we just care that
        // the resolver refused the solution rather than picking a kit
        // whose SDK constraint contradicts the project's SDK pin.
        let msg = err.to_string();
        assert!(
            msg.contains("resolution") || msg.contains("Resolution") || msg.contains("sdk"),
            "unexpected error: {msg}"
        );
    }
}
