use dashmap::DashMap;
use resolvo::utils::Pool;
use resolvo::{
    Candidates, Dependencies, DependencyProvider, Interner, KnownDependencies, NameId, Problem,
    Requirement, SolvableId, Solver, StringId, UnsolvableOrCancelled, VersionSetId,
    VersionSetUnionId,
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
            requirements.push(requirement);
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

    pub async fn build_db(&self, name: &str) -> Result<()> {
        for entry in self.vendors.iter() {
            let vendor_name = entry.key();
            let vendor = entry.value();
            let version_set = vendor.get_options(name).await?;
            let name_id = self.pool.intern_package_name(name.to_string());
            let mut edo_versions = Vec::new();
            for version in version_set {
                let edo_version = EdoVersion::new(vendor_name, &version);
                self.pool.intern_solvable(name_id, edo_version.clone());
                edo_versions.push(edo_version.clone());
            }
            let vsid = self
                .pool
                .intern_version_set(name_id, EdoVersionSet::new(edo_versions.as_slice()));
            if let Some(entry) = self.name_to_vs.get(&name_id) {
                let union_id = match entry.value() {
                    Set::Union(union_id) => {
                        let vs_union = self.pool.resolve_version_set_union(*union_id);
                        self.pool.intern_version_set_union(vsid, vs_union)
                    }
                    Set::Single(vs_id) => self
                        .pool
                        .intern_version_set_union(vsid, [*vs_id].iter().cloned()),
                };
                self.name_to_vs.insert(name_id, Set::Union(union_id));
            } else {
                self.name_to_vs.insert(name_id, Set::Single(vsid));
            }
        }
        Ok(())
    }

    pub fn add_vendor(&mut self, name: &str, vendor: Vendor) {
        self.vendors.insert(name.to_string(), vendor);
    }

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
        solvables.sort_by(|x, y| {
            let left = self.pool.resolve_solvable(*x);
            let right = self.pool.resolve_solvable(*y);
            left.record.version().cmp(&right.record.version())
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
                    known.requirements.push(Requirement::Single(vs_id));
                }
            }
            dependencies = Dependencies::Known(known);
        }

        dependencies
    }
}
