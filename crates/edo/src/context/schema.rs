//! Typed TOML schema for `edo.toml` project files.
//!
//! [`Schema`] is the top-level structure parsed directly from a project's
//! `edo.toml`: it holds free-form `config` keys plus typed sections for
//! caches, environments, sources, transforms, vendors, and dependency
//! requirements. [`Element`] is the shared shape for every plugin
//! definition (a `kind` string, an optional [`SourceMap`], and an
//! arbitrary key/value config table). [`Cache`] groups the three cache
//! categories (source, build, output). [`Requirement`] models a single
//! `[requires.<addr>]` entry. [`SourceDefinition`]/[`SourceMap`] cover
//! the three TOML shapes accepted for an element's `source` field:
//! a single address, a list of addresses, or a map from scope to address
//! list.

use crate::context::{Addr, ContextResult, Element};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single `[requires.<addr>]` entry: a kind plus a semver constraint.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Requirement {
    /// Source kind that satisfies this requirement (e.g. `"image"`).
    pub kind: String,
    /// Semver constraint the resolved version must satisfy.
    pub at: VersionReq,
}

/// Groups the three cache categories — source, build, output — under the
/// `[cache]` table in `edo.toml`.
///
/// Internal-only: callers go through [`Schema`] accessors
/// (`get_source_caches`, `get_build_cache`, `get_output_cache`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct Cache {
    #[serde(default)]
    source: BTreeMap<String, Element>,
    #[serde(default)]
    build: Option<Element>,
    #[serde(default)]
    output: Option<Element>,
}

/// Top-level deserialized form of an `edo.toml` project file.
///
/// Holds the union of every section understood by edo. Each section maps
/// addresses to [`Element`]s of the appropriate plugin category. Use
/// [`Schema::with_namespace`] when stitching multiple `edo.toml` files
/// together (one per directory) and [`Schema::resolve_sources`] before
/// handing elements to the plugin layer.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Schema {
    #[serde(default)]
    config: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    cache: Cache,
    #[serde(default)]
    environment: BTreeMap<Addr, Element>,
    #[serde(default)]
    source: BTreeMap<Addr, Element>,
    #[serde(default)]
    transform: BTreeMap<Addr, Element>,
    #[serde(default)]
    vendor: BTreeMap<Addr, Element>,
    #[serde(default)]
    requires: BTreeMap<Addr, Requirement>,
}

impl Schema {
    /// Propagate will ensure the address keys for all elements get set properly downwards
    pub fn propagate(&mut self) {
        for (name, cache) in self.cache.source.iter_mut() {
            // Source caches are always rooted under `//edo-source-cache/<name>`
            // to match the convention used by `Project::build` /
            // `Context::add_cache`.
            cache.addr = Addr::parse(&format!("//edo-source-cache/{name}")).unwrap();
        }
        if let Some(cache) = self.cache.build.as_mut() {
            // The build cache is always //edo-build-cache
            cache.addr = Addr::parse("//edo-build-cache").unwrap();
        }
        if let Some(cache) = self.cache.output.as_mut() {
            // The output cache is always //edo-output-cache
            cache.addr = Addr::parse("//edo-output-cache").unwrap();
        }
        // Now for all our main elements we propage the key down
        for (addr, element) in self
            .environment
            .iter_mut()
            .chain(self.source.iter_mut())
            .chain(self.transform.iter_mut())
            .chain(self.vendor.iter_mut())
        {
            element.addr = addr.clone();
        }
    }

    /// Re-roots every address in this schema under `namespace`.
    ///
    /// Used by the project loader to hoist a child directory's `edo.toml`
    /// into the parent project's address space before merging.
    pub fn with_namespace(&mut self, namespace: &Addr) {
        fn reroot<T>(
            map: &mut BTreeMap<Addr, T>,
            namespace: &Addr,
            mut on_value: impl FnMut(&mut T, &Addr),
        ) {
            let taken = std::mem::take(map);
            *map = taken
                .into_iter()
                .map(|(addr, mut value)| {
                    on_value(&mut value, namespace);
                    (namespace.join(&addr), value)
                })
                .collect();
        }
        reroot(&mut self.environment, namespace, Element::with_namespace);
        reroot(&mut self.source, namespace, Element::with_namespace);
        reroot(&mut self.transform, namespace, Element::with_namespace);
        reroot(&mut self.vendor, namespace, Element::with_namespace);
        reroot(&mut self.requires, namespace, |_, _| {});
    }

    /// Merges `right` into `self`. Entries in `right` overwrite entries in
    /// `self` for the same address or key (last-writer-wins).
    pub fn merge(&mut self, right: &Schema) {
        for (key, value) in right.config.iter() {
            self.config.insert(key.clone(), value.clone());
        }
        for (key, value) in right.get_source_caches() {
            self.cache.source.insert(key.clone(), value.clone());
        }
        if let Some(element) = right.get_build_cache() {
            self.cache.build = Some(element.clone());
        }
        if let Some(element) = right.get_output_cache() {
            self.cache.output = Some(element.clone());
        }
        for (key, value) in right.environments() {
            self.environment.insert(key.clone(), value.clone());
        }
        for (key, value) in right.sources() {
            self.source.insert(key.clone(), value.clone());
        }
        for (key, value) in right.transforms() {
            self.transform.insert(key.clone(), value.clone());
        }
        for (key, value) in right.vendors() {
            self.vendor.insert(key.clone(), value.clone());
        }
        for (key, value) in right.requires() {
            self.requires.insert(key.clone(), value.clone());
        }
    }

    /// Resolves every embedded [`SourceMap`] into concrete [`Element`]
    /// references by looking up each address in the schema's `source`
    /// table.
    ///
    /// Iterates environments and transforms (the two element categories
    /// that take inputs); already-resolved maps and elements without a
    /// `source` field are skipped. Returns the first error encountered.
    pub fn resolve_sources(&mut self) -> ContextResult<()> {
        // Snapshot `source` before mutating other categories so resolution
        // is deterministic and not influenced by iteration order.
        let elements = self.source.clone();
        for element in self
            .environment
            .values_mut()
            .chain(self.transform.values_mut())
        {
            if let Some(sourcemap) = element.source.as_mut() {
                sourcemap.resolve(&elements)?;
            }
        }
        Ok(())
    }

    /// Source cache backends, keyed by the user-provided cache name.
    pub fn get_source_caches(&self) -> &BTreeMap<String, Element> {
        &self.cache.source
    }

    /// Build cache backend, if `[cache.build]` is set.
    pub fn get_build_cache(&self) -> Option<&Element> {
        self.cache.build.as_ref()
    }

    /// Output cache backend, if `[cache.output]` is set.
    pub fn get_output_cache(&self) -> Option<&Element> {
        self.cache.output.as_ref()
    }

    /// Free-form `[config]` entries forwarded to plugins.
    #[allow(dead_code)]
    pub fn get_config(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.config
    }

    /// Environment farm definitions keyed by address.
    pub fn environments(&self) -> &BTreeMap<Addr, Element> {
        &self.environment
    }

    /// Source definitions keyed by address.
    pub fn sources(&self) -> &BTreeMap<Addr, Element> {
        &self.source
    }

    /// Inserts a resolved source element. Used by the lockfile / resolver
    /// path to fold vendor-provided sources back into the schema before
    /// [`Schema::resolve_sources`] runs.
    ///
    /// Normalizes the inserted element's `addr` to match the schema-map
    /// key — vendor-resolved elements come in with their original
    /// vendor-internal address, which must be rewritten so downstream
    /// consumers (notably `cargo_vendor`'s `BTreeMap<Addr, Source>`) key
    /// off the project address rather than the vendor address.
    pub fn add_source(&mut self, addr: &Addr, element: &Element) {
        let mut element = element.clone();
        element.addr = addr.clone();
        self.source.insert(addr.clone(), element);
    }

    /// Transform definitions keyed by address.
    pub fn transforms(&self) -> &BTreeMap<Addr, Element> {
        &self.transform
    }

    /// Vendor definitions keyed by address.
    pub fn vendors(&self) -> &BTreeMap<Addr, Element> {
        &self.vendor
    }

    /// Dependency requirements keyed by address.
    pub fn requires(&self) -> &BTreeMap<Addr, Requirement> {
        &self.requires
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the typed schema. Each test feeds a small `edo.toml`
    //! fragment to `toml::from_str` and asserts on the resulting [`Schema`]
    //! to lock down the deserialization shape.
    use super::*;
    use crate::context::{SourceMap, error};

    fn addr(s: &str) -> Addr {
        Addr::parse(s).unwrap()
    }

    #[test]
    fn deserialize_minimal_schema_is_empty() {
        let s: Schema = toml::from_str("").unwrap();
        assert!(s.environments().is_empty());
        assert!(s.sources().is_empty());
        assert!(s.transforms().is_empty());
        assert!(s.vendors().is_empty());
        assert!(s.requires().is_empty());
        assert!(s.get_source_caches().is_empty());
        assert!(s.get_build_cache().is_none());
        assert!(s.get_output_cache().is_none());
    }

    #[test]
    fn deserialize_populates_all_sections() {
        let toml_str = r#"
[source."//foo"]
kind = "local"
path = "x"

[transform."//t"]
kind = "script"
source = "//foo"

[vendor."//v"]
kind = "image"

[environment."//env"]
kind = "local"

[cache.source.c]
kind = "local"
path = "/tmp"

[cache.build]
kind = "local"
path = "/tmp/b"

[cache.output]
kind = "local"
path = "/tmp/o"

[requires."//bar"]
kind = "image"
at = "=1.0.0"
"#;
        let s: Schema = toml::from_str(toml_str).expect("parse schema");

        assert!(s.sources().contains_key(&addr("//foo")));
        assert!(s.transforms().contains_key(&addr("//t")));
        assert!(s.vendors().contains_key(&addr("//v")));
        assert!(s.environments().contains_key(&addr("//env")));
        assert!(s.get_source_caches().contains_key("c"));
        assert!(s.get_build_cache().is_some());
        assert!(s.get_output_cache().is_some());
        assert!(s.requires().contains_key(&addr("//bar")));
    }

    #[test]
    fn element_flattens_unknown_keys_into_config() {
        let toml_str = r#"
kind = "local"
path = "x"
extra = 42
"#;
        let e: Element = toml::from_str(toml_str).unwrap();
        assert_eq!(e.kind, "local");
        assert_eq!(e.config.get("path").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(e.config.get("extra").and_then(|v| v.as_i64()), Some(42));
    }

    #[test]
    fn source_definition_single_normalizes_to_default_scope() {
        let toml_str = "kind = \"script\"\nsource = \"//foo\"\n";
        let e: Element = toml::from_str(toml_str).unwrap();
        let sm = e.source.expect("source set");
        match sm {
            SourceMap::Unresolved(table) => {
                assert_eq!(table.len(), 1);
                assert_eq!(table["default"], vec![addr("//foo")]);
            }
            SourceMap::Resolved(_) => panic!("expected unresolved"),
        }
    }

    #[test]
    fn source_definition_list_normalizes_to_default_scope() {
        let toml_str = "kind = \"script\"\nsource = [\"//a\", \"//b\"]\n";
        let e: Element = toml::from_str(toml_str).unwrap();
        match e.source.unwrap() {
            SourceMap::Unresolved(table) => {
                assert_eq!(table["default"], vec![addr("//a"), addr("//b")]);
            }
            SourceMap::Resolved(_) => panic!("expected unresolved"),
        }
    }

    #[test]
    fn source_definition_table_keeps_scope_keys() {
        let toml_str = r#"
kind = "script"
[source]
build = ["//a"]
runtime = ["//b", "//c"]
"#;
        let e: Element = toml::from_str(toml_str).unwrap();
        match e.source.unwrap() {
            SourceMap::Unresolved(table) => {
                assert_eq!(table["build"], vec![addr("//a")]);
                assert_eq!(table["runtime"], vec![addr("//b"), addr("//c")]);
            }
            SourceMap::Resolved(_) => panic!("expected unresolved"),
        }
    }

    #[test]
    fn with_namespace_reroots_relative_source_addresses() {
        // A relative source address (no `//` prefix) is hoisted under the
        // namespace; absolute ones pass through Addr::join unchanged.
        let toml_str = r#"
kind = "script"
source = ["rel", "//abs"]
"#;
        let mut e: Element = toml::from_str(toml_str).unwrap();
        e.with_namespace(&addr("//ns"));
        match e.source.unwrap() {
            SourceMap::Unresolved(table) => {
                let v = &table["default"];
                assert_eq!(v[0], addr("//ns/rel"));
                assert_eq!(v[1], addr("//abs"));
            }
            SourceMap::Resolved(_) => panic!("expected unresolved"),
        }
    }

    #[test]
    fn schema_with_namespace_reroots_section_keys() {
        let toml_str = r#"
[source."foo"]
kind = "local"
path = "x"

[transform."t"]
kind = "script"
"#;
        let mut s: Schema = toml::from_str(toml_str).unwrap();
        s.with_namespace(&addr("//ns"));
        assert!(s.sources().contains_key(&addr("//ns/foo")));
        assert!(s.transforms().contains_key(&addr("//ns/t")));
    }

    #[test]
    fn merge_overwrites_overlapping_addresses() {
        let mut left: Schema =
            toml::from_str("[source.\"//a\"]\nkind = \"local\"\npath = \"old\"\n").unwrap();
        let right: Schema = toml::from_str(
            "[source.\"//a\"]\nkind = \"local\"\npath = \"new\"\n\n[source.\"//b\"]\nkind = \"local\"\npath = \"y\"\n",
        )
        .unwrap();
        left.merge(&right);
        let a = &left.sources()[&addr("//a")];
        assert_eq!(a.config.get("path").and_then(|v| v.as_str()), Some("new"),);
        assert!(left.sources().contains_key(&addr("//b")));
    }

    #[test]
    fn resolve_sources_replaces_addresses_with_elements() {
        let toml_str = r#"
[source."//foo"]
kind = "local"
path = "x"

[transform."//t"]
kind = "script"
source = "//foo"
"#;
        let mut s: Schema = toml::from_str(toml_str).unwrap();
        s.resolve_sources().expect("resolve ok");
        let t = &s.transforms()[&addr("//t")];
        let resolved = t
            .source
            .as_ref()
            .and_then(|sm| sm.get_resolved())
            .expect("resolved");
        assert_eq!(resolved["default"].len(), 1);
        assert_eq!(resolved["default"][0].kind, "local");
    }

    #[test]
    fn resolve_sources_missing_address_returns_missing_source() {
        let toml_str = r#"
[transform."//t"]
kind = "script"
source = "//missing"
"#;
        let mut s: Schema = toml::from_str(toml_str).unwrap();
        let err = s.resolve_sources().expect_err("should fail");
        assert!(
            matches!(err, error::ContextError::MissingSource { .. }),
            "got: {err:?}",
        );
    }

    #[test]
    fn resolve_sources_idempotent_on_resolved_map() {
        let toml_str = r#"
[source."//foo"]
kind = "local"
path = "x"

[transform."//t"]
kind = "script"
source = "//foo"
"#;
        let mut s: Schema = toml::from_str(toml_str).unwrap();
        s.resolve_sources().unwrap();
        // Second call must succeed and leave the map resolved.
        s.resolve_sources().unwrap();
        let t = &s.transforms()[&addr("//t")];
        assert!(t.source.as_ref().and_then(|sm| sm.get_resolved()).is_some());
    }
}
