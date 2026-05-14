//! Project loading and build orchestration.
//!
//! This module contains [`Project`], which walks a directory tree for `edo.toml`
//! files, resolves dependencies through vendors, manages the lock file, and
//! registers plugins, environments, and transforms with the [`super::Context`].
//! It also re-exports the [`non_configurable!`] and
//! [`non_configurable_no_context!`] convenience macros.

use super::Context;
use super::address::Addr;
use super::lock::Lock;
use super::{ContextResult as Result, FromNode, Node, error};
use crate::context::schema::Schema;
use crate::source::{Dependency, Resolver};
use snafu::{OptionExt, ResultExt};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, read, read_dir};
use std::path::{Path, PathBuf};

/// Intermediate representation of a loaded edo project.
///
/// Holds the parsed configuration nodes collected from `edo.toml` files before
/// they are resolved and registered with the [`Context`].
pub struct Project {
    project_path: PathBuf,
    config_nodes: BTreeMap<String, Node>,
    source_caches: BTreeMap<Addr, Node>,
    build_cache: Option<Node>,
    output_cache: Option<Node>,
    vendors: BTreeMap<Addr, Node>,
    environments: BTreeMap<Addr, Node>,
    transforms: BTreeMap<Addr, Node>,
    need_resolution: BTreeMap<Addr, Node>,
}

fn handle_sources(namespace: &Addr, node: &Node, _sources: &BTreeMap<Addr, Node>) -> Result<Node> {
    let id = node.get_id().context(error::NodeSnafu)?;
    let kind = node.get_kind().context(error::NodeSnafu)?;
    let name = node.get_name().context(error::NodeSnafu)?;
    let mut table = node.get_table().context(error::NodeSnafu)?;

    if let Some(src) = table.get("source") {
        if let Some(addr) = src.as_string() {
            let caddr = if addr.starts_with("//") {
                Addr::parse(&addr)?
            } else {
                namespace.join(&addr)
            };
            info!("checking for source at: {caddr}");
            table.insert("source".into(), Node::new_string(caddr.to_string()));
        } else if let Some(list) = src.as_list() {
            let mut items = Vec::new();
            for item in list.iter() {
                let addr = item.as_string().context(error::FieldSnafu {
                    field: "requires",
                    type_: "array of string / string",
                })?;
                let caddr = if addr.starts_with("//") {
                    Addr::parse(&addr)?
                } else {
                    namespace.join(&addr)
                };
                items.push(Node::new_string(caddr.to_string()));
            }
            table.insert("source".into(), Node::new_list(items));
        }
    }

    Ok(Node::new_definition(&id, &kind, &name, table))
}

impl Project {
    fn calculate_digest(&self) -> Result<String> {
        let mut hasher = blake3::Hasher::new();
        for (key, value) in self.need_resolution.iter() {
            hasher.update(key.to_string().as_bytes());
            let bytes = serde_json::to_vec(value).context(error::SerializeSnafu)?;
            hasher.update(bytes.as_slice());
        }
        let digest = hasher.finalize();
        Ok(base16::encode_lower(digest.as_bytes()))
    }

    /// Loads all `edo.toml` files under `path`, resolves dependencies, and registers
    /// plugins, environments, and transforms with the given [`Context`].
    pub async fn load<P: AsRef<Path>>(path: P, ctx: &Context, error_on_lock: bool) -> Result<()> {
        let mut project = Self {
            project_path: path.as_ref().to_path_buf(),
            config_nodes: BTreeMap::new(),
            source_caches: BTreeMap::new(),
            build_cache: None,
            output_cache: None,
            vendors: BTreeMap::new(),
            environments: BTreeMap::new(),
            transforms: BTreeMap::new(),
            need_resolution: BTreeMap::new(),
        };
        let mut sources = BTreeMap::new();
        project.walk(&Addr::default(), path.as_ref(), &mut sources)?;
        project.resolve_sources(&sources)?;
        project.build(ctx, error_on_lock).await?;
        Ok(())
    }

    fn walk(
        &mut self,
        namespace: &Addr,
        directory: &Path,
        sources: &mut BTreeMap<Addr, Node>,
    ) -> Result<()> {
        let read = read_dir(directory).context(error::IoSnafu)?;
        for entry in read {
            let entry = entry.context(error::IoSnafu)?;
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|x| x.to_str()).unwrap() == "edo.toml" {
                // This is a barkml defined build file
                sources.extend(self.load_toml(namespace, &path)?);
            } else if path.is_dir() {
                let dir_name = path.file_name().and_then(|x| x.to_str()).unwrap();
                let addr = namespace.join(dir_name);
                self.walk(&addr, &path, sources)?;
            }
        }
        Ok(())
    }

    fn resolve_sources(&mut self, sources: &BTreeMap<Addr, Node>) -> Result<()> {
        for (name, node) in self
            .environments
            .iter_mut()
            .chain(self.transforms.iter_mut())
        {
            debug!(component = "project", "mapping sources for element {name}");
            let mut table = node.get_table().context(error::FieldSnafu {
                field: "environment",
                type_: "table",
            })?;
            if let Some(source) = table.get("source") {
                if let Some(list) = source.as_list() {
                    let mut items = Vec::new();
                    for entry in list.iter() {
                        let addr = Addr::parse(&entry.as_string().context(error::FieldSnafu {
                            field: "source",
                            type_: "string",
                        })?)?;
                        items.push(
                            sources
                                .get(&addr)
                                .context(error::NotValidSourceSnafu {
                                    id: addr.to_string(),
                                })?
                                .clone(),
                        );
                    }
                    table.insert("source".to_string(), Node::new_list(items));
                } else {
                    let addr = Addr::parse(&source.as_string().context(error::FieldSnafu {
                        field: "source",
                        type_: "string",
                    })?)?;
                    table.insert(
                        "source".to_string(),
                        sources
                            .get(&addr)
                            .context(error::NotValidSourceSnafu {
                                id: addr.to_string(),
                            })?
                            .clone(),
                    );
                }
            }
            node.set_table(table);
        }
        Ok(())
    }

    fn load_toml(&mut self, namespace: &Addr, file: &Path) -> Result<BTreeMap<Addr, Node>> {
        debug!(component = "project", "loading transforms from {file:?}");
        let config_bytes = read(file).context(error::IoSnafu)?;
        let config: Schema = toml::from_slice(&config_bytes).context(error::DeserializeSnafu)?;
        match config {
            Schema::V1(config) => {
                for (name, node) in config.get_config()? {
                    self.config_nodes.insert(name, node);
                }
                let mut sources = BTreeMap::new();
                for (name, node) in config.get_sources()? {
                    let addr = namespace.join(&name);
                    sources.insert(addr, node);
                }
                for (name, node) in config.get_requires()? {
                    let addr = namespace.join(&name);
                    self.need_resolution.insert(addr.clone(), node.clone());
                    sources.insert(addr, node);
                }
                for (name, node) in config.get_source_caches()? {
                    let addr = namespace.join(&name);
                    self.source_caches.insert(addr, node.clone());
                }
                self.build_cache = config.get_build_cache()?;
                self.output_cache = config.get_output_cache()?;
                for (name, node) in config.get_environments()? {
                    let addr = namespace.join(&name);
                    let cnode = handle_sources(namespace, &node, &sources)?;
                    self.environments.insert(addr, cnode);
                }
                for (name, node) in config.get_transforms()? {
                    let addr = namespace.join(&name);
                    let cnode = handle_sources(namespace, &node, &sources)?;
                    self.transforms.insert(addr, cnode);
                }
                for (name, node) in config.get_vendors()? {
                    let addr = namespace.join(&name);
                    self.vendors.insert(addr, node);
                }
                Ok(sources)
            }
        }
    }

    /// Resolves dependencies, registers plugins/environments/transforms, and
    /// writes the lock file.
    pub async fn build(&mut self, ctx: &Context, error_on_lock: bool) -> Result<()> {
        // Calculate the digest of the project configuration
        let digest = self.calculate_digest()?;
        ctx.add_config(&self.config_nodes);
        // Check for an existing lockfile
        let lock_file = self.project_path.join("edo.lock.json");
        if lock_file.exists() {
            let mut file = File::open(&lock_file).context(error::IoSnafu)?;
            let lock: Lock = serde_json::from_reader(&mut file).context(error::SerializeSnafu)?;
            // Now check if the digests match, if so then we should use the lockfile to resolve our unresolved nodes
            if lock.digest() == digest {
                info!(target: "project", "no changes detected in project, reusing lock resolution file");
                for (addr, node) in self.need_resolution.iter() {
                    let resolved = lock
                        .content()
                        .get(addr)
                        .context(error::MalformedLockSnafu { addr: addr.clone() })?;
                    node.set_data(&resolved.data());
                }
                for (addr, node) in self.environments.iter() {
                    ctx.add_farm(addr, node).await?;
                }

                for (addr, node) in self.transforms.iter() {
                    ctx.add_transform(addr, node).await?;
                }
                return Ok(());
            } else if lock.digest() != digest && error_on_lock {
                return error::DependencyChangeSnafu {}.fail();
            }
        }

        // Resolve all storage backends
        for (addr, node) in self.source_caches.iter() {
            ctx.add_cache(addr, node).await?;
        }
        if let Some(node) = self.build_cache.as_ref() {
            ctx.add_cache(&Addr::parse("//edo-build-cache")?, node)
                .await?;
        }
        if let Some(node) = self.output_cache.as_ref() {
            ctx.add_cache(&Addr::parse("//edo-output-cache")?, node)
                .await?;
        }

        // Vendor's are only used during project resolution
        // Now we should create a resolver
        let mut resolver = Resolver::default();
        let mut vendors = HashMap::new();
        // Register all our vendors
        for (addr, node) in self.vendors.iter() {
            let vendor = ctx.add_vendor(addr, node).await?;
            vendors.insert(addr.to_string(), vendor.clone());
            debug!(
                section = "context",
                component = "project",
                "register vendor {addr}"
            );
            resolver.add_vendor(&addr.to_string(), vendor.clone());
        }

        // Now for every node needing resolution we need to get the vendor field to resolve
        let mut need_resolution = Vec::new();
        let mut assigners = HashMap::new();
        for (addr, node) in self.need_resolution.iter() {
            debug!(
                section = "context",
                component = "project",
                "{addr} needs resolution"
            );
            let dep = Dependency::from_node(addr, node, ctx).await?;
            assigners.insert(dep.addr.clone(), node.clone());
            // Populate the resolver for this dependency
            resolver.build_db(dep.name.as_str()).await?;
            need_resolution.push(dep);
        }

        // Now that we have built the databases we want to run the resolution
        // unfortunately due to resolvo using its own async through rayno hidden behind only
        // synchronous calls we have to use spawn_blocking here
        let resolved = tokio::task::spawn_blocking(move || resolver.resolve(need_resolution))
            .await
            .unwrap()?;

        // Create the new lock
        let mut lock = Lock::new(digest);

        for (addr, (vendor_name, name, version)) in resolved.iter() {
            debug!(
                section = "context",
                component = "project",
                "resolved {addr} to {name}@{version} from vendor {vendor_name}"
            );
            let vendor = vendors.get(vendor_name).unwrap();
            let target = assigners.get(addr).unwrap();
            let resolved = vendor.resolve(name, version).await?;
            lock.content_mut().insert(addr.clone(), resolved.clone());
            target.set_data(&resolved.data());
        }

        for (addr, node) in self.environments.iter() {
            debug!(
                section = "context",
                component = "project",
                "adding environment farm {addr}"
            );
            ctx.add_farm(addr, node).await?;
        }

        for (addr, node) in self.transforms.iter() {
            debug!(
                section = "context",
                component = "project",
                "adding transform {addr}"
            );
            ctx.add_transform(addr, node).await?;
        }

        // Write out the lock file
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.project_path.join("edo.lock.json"))
            .context(error::IoSnafu)?;

        serde_json::to_writer_pretty(&mut file, &lock).context(error::SerializeSnafu)?;
        Ok(())
    }
}

/// Implements [`Definable`](crate::context::Definable) as a no-op for types that require no configuration.
#[macro_export]
macro_rules! non_configurable {
    ($ty: ident, $e: ty) => {
        impl $crate::context::Definable<$e, $crate::context::NonConfigurable<$e>> for $ty {
            fn key() -> &'static str {
                "noop"
            }

            fn set_config(
                &mut self,
                _: &$crate::context::NonConfigurable<$e>,
            ) -> std::result::Result<(), $e> {
                Ok(())
            }
        }
    };
}

/// Implements [`DefinableNoContext`](crate::context::DefinableNoContext) as a no-op for types that require no configuration.
#[macro_export]
macro_rules! non_configurable_no_context {
    ($ty: ident, $e: ty) => {
        impl $crate::context::DefinableNoContext<$e, $crate::context::NonConfigurable<$e>> for $ty {
            fn key() -> &'static str {
                "noop"
            }

            fn set_config(
                &mut self,
                _: &$crate::context::NonConfigurable<$e>,
            ) -> std::result::Result<(), $e> {
                Ok(())
            }
        }
    };
}

pub use non_configurable;
pub use non_configurable_no_context;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn addr(s: &str) -> Addr {
        Addr::parse(s).unwrap()
    }

    fn def_node(id: &str, kind: &str, name: &str, table: BTreeMap<String, Node>) -> Node {
        Node::new_definition(id, kind, name, table)
    }

    fn write_edo_toml(dir: &Path, content: &str) {
        std::fs::write(dir.join("edo.toml"), content).expect("write edo.toml");
    }

    fn empty_project(path: &Path) -> Project {
        Project {
            project_path: path.to_path_buf(),
            config_nodes: BTreeMap::new(),
            source_caches: BTreeMap::new(),
            build_cache: None,
            output_cache: None,
            vendors: BTreeMap::new(),
            environments: BTreeMap::new(),
            transforms: BTreeMap::new(),
            need_resolution: BTreeMap::new(),
        }
    }

    // ── handle_sources tests ─────────────────────────────────────────────────

    /// An absolute address (starts with "//") is kept as-is.
    #[test]
    fn handle_sources_absolute_address_kept_as_is() {
        let namespace = addr("//ns");
        let mut table = BTreeMap::new();
        table.insert(
            "source".to_string(),
            Node::new_string("//abs/addr".to_string()),
        );
        let node = def_node("transform", "script", "build", table);
        let result = handle_sources(&namespace, &node, &BTreeMap::new());
        let out = result.expect("handle_sources should succeed");
        let t = out.get_table().unwrap();
        assert_eq!(
            t.get("source").unwrap().as_string().as_deref(),
            Some("//abs/addr"),
        );
    }

    /// A relative address (no "//" prefix) is joined with the namespace.
    #[test]
    fn handle_sources_relative_address_joined() {
        let namespace = addr("//ns");
        let mut table = BTreeMap::new();
        table.insert(
            "source".to_string(),
            Node::new_string("relative".to_string()),
        );
        let node = def_node("transform", "script", "build", table);
        let result = handle_sources(&namespace, &node, &BTreeMap::new());
        let out = result.expect("handle_sources should succeed");
        let t = out.get_table().unwrap();
        assert_eq!(
            t.get("source").unwrap().as_string().as_deref(),
            Some("//ns/relative"),
        );
    }

    /// A list source mixes absolute and relative elements correctly.
    #[test]
    fn handle_sources_list_mixes_absolute_and_relative() {
        let namespace = addr("//ns");
        let list = Node::new_list(vec![
            Node::new_string("a".to_string()),
            Node::new_string("//b/c".to_string()),
        ]);
        let mut table = BTreeMap::new();
        table.insert("source".to_string(), list);
        let node = def_node("transform", "script", "build", table);
        let out = handle_sources(&namespace, &node, &BTreeMap::new())
            .expect("handle_sources should succeed");
        let t = out.get_table().unwrap();
        let items = t.get("source").unwrap().as_list().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_string().as_deref(), Some("//ns/a"));
        assert_eq!(items[1].as_string().as_deref(), Some("//b/c"));
    }

    /// A list element that is not a string produces a Field error.
    #[test]
    fn handle_sources_list_non_string_element_errors() {
        let namespace = addr("//ns");
        let list = Node::new_list(vec![Node::new_int(123)]);
        let mut table = BTreeMap::new();
        table.insert("source".to_string(), list);
        let node = def_node("transform", "script", "build", table);
        let result = handle_sources(&namespace, &node, &BTreeMap::new());
        match result {
            Err(error::ContextError::Field { field, .. }) => {
                assert_eq!(field, "requires");
            }
            other => panic!("expected Field error, got: {other:?}"),
        }
    }

    /// A non-Definition node (e.g. bool) produces a Node error.
    #[test]
    fn handle_sources_missing_id_kind_name_errors() {
        let namespace = addr("//ns");
        let node = Node::new_bool(true);
        let result = handle_sources(&namespace, &node, &BTreeMap::new());
        assert!(
            matches!(result, Err(error::ContextError::Node)),
            "expected Node error, got: {result:?}",
        );
    }

    /// A definition node without a "source" key passes through unchanged.
    #[test]
    fn handle_sources_without_source_passes_through() {
        let namespace = addr("//ns");
        let mut table = BTreeMap::new();
        table.insert("key".to_string(), Node::new_string("val".to_string()));
        let node = def_node("transform", "script", "build", table);
        let out = handle_sources(&namespace, &node, &BTreeMap::new())
            .expect("handle_sources should succeed");
        let t = out.get_table().unwrap();
        assert!(t.contains_key("key"));
        assert!(!t.contains_key("source"));
    }

    // ── Project::calculate_digest tests ──────────────────────────────────────

    /// The digest is non-empty and contains only lowercase hex characters.
    #[test]
    fn calculate_digest_is_lowercase_hex() {
        let dir = TempDir::new().unwrap();
        let mut project = empty_project(dir.path());
        project
            .need_resolution
            .insert(addr("//x"), Node::new_string("a".to_string()));
        let digest = project.calculate_digest().expect("digest ok");
        assert!(!digest.is_empty(), "digest must be non-empty");
        assert!(
            digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "digest must be lowercase hex, got: {digest}",
        );
    }

    /// The same need_resolution content always produces the same digest.
    #[test]
    fn calculate_digest_stable_for_same_input() {
        let dir = TempDir::new().unwrap();
        let mut p1 = empty_project(dir.path());
        p1.need_resolution
            .insert(addr("//x"), Node::new_string("same".to_string()));
        let mut p2 = empty_project(dir.path());
        p2.need_resolution
            .insert(addr("//x"), Node::new_string("same".to_string()));
        assert_eq!(
            p1.calculate_digest().unwrap(),
            p2.calculate_digest().unwrap(),
        );
    }

    /// Different need_resolution values produce different digests.
    #[test]
    fn calculate_digest_changes_with_content() {
        let dir = TempDir::new().unwrap();
        let mut p1 = empty_project(dir.path());
        p1.need_resolution
            .insert(addr("//x"), Node::new_string("a".to_string()));
        let mut p2 = empty_project(dir.path());
        p2.need_resolution
            .insert(addr("//x"), Node::new_string("b".to_string()));
        assert_ne!(
            p1.calculate_digest().unwrap(),
            p2.calculate_digest().unwrap(),
        );
    }

    // ── Project::load_toml tests ──────────────────────────────────────────────

    /// load_toml populates all major sections from a complete edo.toml.
    #[test]
    fn load_toml_populates_all_sections() {
        let dir = TempDir::new().unwrap();
        let content = r#"
schema-version = "1"

[source.foo]
kind = "local"
path = "x"

[transform.t]
kind = "script"
source = "foo"

[vendor.v]
kind = "image"

[cache.source.c]
kind = "local"
path = "/tmp"

[cache.build]
kind = "local"
path = "/tmp/b"

[cache.output]
kind = "local"
path = "/tmp/o"

[requires.bar]
kind = "image"
at = "=1.0.0"
"#;
        write_edo_toml(dir.path(), content);
        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let sources = project
            .load_toml(&ns, &dir.path().join("edo.toml"))
            .expect("load_toml ok");

        // sources map must contain the regular source "foo" and "bar" (requires)
        assert!(
            sources.contains_key(&ns.join("foo")),
            "sources must contain 'foo'",
        );
        assert!(
            sources.contains_key(&ns.join("bar")),
            "sources must contain 'bar' (requires)",
        );

        // transforms
        assert!(
            project.transforms.contains_key(&ns.join("t")),
            "transforms must contain 't'",
        );

        // vendors
        assert!(
            project.vendors.contains_key(&ns.join("v")),
            "vendors must contain 'v'",
        );

        // source caches
        assert!(
            project.source_caches.contains_key(&ns.join("c")),
            "source_caches must contain 'c'",
        );

        // build / output caches
        assert!(project.build_cache.is_some(), "build_cache must be Some");
        assert!(project.output_cache.is_some(), "output_cache must be Some");

        // need_resolution
        assert!(
            project.need_resolution.contains_key(&ns.join("bar")),
            "need_resolution must contain 'bar'",
        );
    }

    /// Malformed TOML returns a Deserialize error.
    #[test]
    fn load_toml_malformed_toml_errors() {
        let dir = TempDir::new().unwrap();
        write_edo_toml(dir.path(), "this is = not = valid");
        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let result = project.load_toml(&ns, &dir.path().join("edo.toml"));
        assert!(
            matches!(result, Err(error::ContextError::Deserialize { .. })),
            "expected Deserialize error, got: {result:?}",
        );
    }

    /// A source block without a `kind` field returns a Field error.
    #[test]
    fn load_toml_missing_kind_errors() {
        let dir = TempDir::new().unwrap();
        // source.foo has no `kind`
        let content = "schema-version = \"1\"\n[source.foo]\npath = \"x\"\n";
        write_edo_toml(dir.path(), content);
        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let result = project.load_toml(&ns, &dir.path().join("edo.toml"));
        match result {
            Err(error::ContextError::Field { ref field, .. }) => {
                assert_eq!(field, "kind");
            }
            other => panic!("expected Field {{field: \"kind\", ..}}, got: {other:?}"),
        }
    }

    // ── Project::walk tests ───────────────────────────────────────────────────

    /// walk collects edo.toml from the root and a subdirectory.
    #[test]
    fn walk_collects_edo_toml_from_subdirs() {
        let dir = TempDir::new().unwrap();
        let root_content = "schema-version = \"1\"\n[source.a]\nkind = \"local\"\npath = \"x\"\n";
        write_edo_toml(dir.path(), root_content);

        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_content = "schema-version = \"1\"\n[source.b]\nkind = \"local\"\npath = \"y\"\n";
        std::fs::write(sub.join("edo.toml"), sub_content).unwrap();

        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let mut sources = BTreeMap::new();
        project
            .walk(&ns, dir.path(), &mut sources)
            .expect("walk ok");

        assert!(
            sources.contains_key(&ns.join("a")),
            "sources must contain 'a' (root)",
        );
        assert!(
            sources.contains_key(&ns.join("sub").join("b")),
            "sources must contain 'sub/b'",
        );
    }

    /// walk on an empty directory succeeds and adds nothing to sources.
    #[test]
    fn walk_empty_directory_is_ok() {
        let dir = TempDir::new().unwrap();
        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let mut sources = BTreeMap::new();
        project
            .walk(&ns, dir.path(), &mut sources)
            .expect("walk ok");
        assert!(sources.is_empty(), "sources must be empty for empty dir");
    }

    /// walk ignores non-edo.toml files.
    #[test]
    fn walk_skips_non_edo_toml_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("other.toml"), "schema-version = \"1\"\n").unwrap();
        let ns = Addr::default();
        let mut project = empty_project(dir.path());
        let mut sources = BTreeMap::new();
        project
            .walk(&ns, dir.path(), &mut sources)
            .expect("walk ok");
        assert!(sources.is_empty(), "sources must be empty (no edo.toml)");
    }

    // ── Project::resolve_sources tests ───────────────────────────────────────

    /// resolve_sources rewrites a scalar "source" string to the actual source node.
    #[test]
    fn resolve_sources_rewrites_scalar_source() {
        let dir = TempDir::new().unwrap();
        let src_addr = addr("//ns/s");
        let real_node = Node::new_string("real source".to_string());
        let mut sources = BTreeMap::new();
        sources.insert(src_addr.clone(), real_node.clone());

        let mut table = BTreeMap::new();
        table.insert("source".to_string(), Node::new_string(src_addr.to_string()));
        let transform_node = def_node("transform", "script", "t", table);

        let mut project = empty_project(dir.path());
        project.transforms.insert(addr("//t"), transform_node);
        project
            .resolve_sources(&sources)
            .expect("resolve_sources ok");

        let t = project.transforms.get(&addr("//t")).unwrap();
        let tbl = t.get_table().unwrap();
        assert_eq!(
            tbl.get("source").unwrap().as_string().as_deref(),
            Some("real source"),
        );
    }

    /// resolve_sources rewrites a list of source addresses to actual nodes.
    #[test]
    fn resolve_sources_rewrites_list_source() {
        let dir = TempDir::new().unwrap();
        let sa = addr("//ns/a");
        let sb = addr("//ns/b");
        let node_a = Node::new_string("node-a".to_string());
        let node_b = Node::new_string("node-b".to_string());
        let mut sources = BTreeMap::new();
        sources.insert(sa.clone(), node_a);
        sources.insert(sb.clone(), node_b);

        let list = Node::new_list(vec![
            Node::new_string(sa.to_string()),
            Node::new_string(sb.to_string()),
        ]);
        let mut table = BTreeMap::new();
        table.insert("source".to_string(), list);
        let transform_node = def_node("transform", "script", "t", table);

        let mut project = empty_project(dir.path());
        project.transforms.insert(addr("//t"), transform_node);
        project
            .resolve_sources(&sources)
            .expect("resolve_sources ok");

        let t = project.transforms.get(&addr("//t")).unwrap();
        let tbl = t.get_table().unwrap();
        let items = tbl.get("source").unwrap().as_list().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_string().as_deref(), Some("node-a"));
        assert_eq!(items[1].as_string().as_deref(), Some("node-b"));
    }

    /// resolve_sources returns NotValidSource when the referenced addr is absent.
    #[test]
    fn resolve_sources_missing_source_errors() {
        let dir = TempDir::new().unwrap();
        let mut table = BTreeMap::new();
        table.insert(
            "source".to_string(),
            Node::new_string("//missing".to_string()),
        );
        let transform_node = def_node("transform", "script", "t", table);
        let mut project = empty_project(dir.path());
        project.transforms.insert(addr("//t"), transform_node);
        let result = project.resolve_sources(&BTreeMap::new());
        match result {
            Err(error::ContextError::NotValidSource { id }) => {
                assert!(
                    id.contains("missing"),
                    "id should mention 'missing', got: {id}"
                );
            }
            other => panic!("expected NotValidSource, got: {other:?}"),
        }
    }

    /// resolve_sources returns a Field error when "source" is not a string.
    #[test]
    fn resolve_sources_wrong_type_errors() {
        let dir = TempDir::new().unwrap();
        let mut table = BTreeMap::new();
        table.insert("source".to_string(), Node::new_int(42));
        let transform_node = def_node("transform", "script", "t", table);
        let mut project = empty_project(dir.path());
        project.transforms.insert(addr("//t"), transform_node);
        let result = project.resolve_sources(&BTreeMap::new());
        assert!(
            matches!(result, Err(error::ContextError::Field { .. })),
            "expected Field error, got: {result:?}",
        );
    }

    // ── Macro tests ───────────────────────────────────────────────────────────

    /// non_configurable! generates a working Definable impl with key() == "noop".
    #[test]
    fn non_configurable_macro_compiles() {
        use async_trait::async_trait;

        #[derive(Default)]
        struct DummyCtx;

        #[derive(Debug)]
        struct DummyCtxErr;

        #[async_trait]
        impl FromNode for DummyCtx {
            type Error = DummyCtxErr;
            async fn from_node(
                _addr: &Addr,
                _node: &Node,
                _ctx: &Context,
            ) -> std::result::Result<Self, DummyCtxErr> {
                Ok(DummyCtx)
            }
        }

        // Invoke the macro — expands to a Definable impl for DummyCtx.
        non_configurable!(DummyCtx, DummyCtxErr);

        // Verify key() and set_config() via the Definable trait.
        assert_eq!(
            <DummyCtx as crate::context::Definable<
                DummyCtxErr,
                crate::context::NonConfigurable<DummyCtxErr>,
            >>::key(),
            "noop",
        );
        let mut d = DummyCtx;
        let cfg = crate::context::NonConfigurable::<DummyCtxErr>::default();
        <DummyCtx as crate::context::Definable<
            DummyCtxErr,
            crate::context::NonConfigurable<DummyCtxErr>,
        >>::set_config(&mut d, &cfg)
        .unwrap();
    }

    /// non_configurable_no_context! generates a working DefinableNoContext impl.
    #[test]
    fn non_configurable_no_context_macro_compiles() {
        use crate::context::{Config, FromNodeNoContext};
        use async_trait::async_trait;

        #[derive(Default)]
        struct DummyNoCtx;

        #[derive(Debug)]
        struct DummyNoCtxErr;

        #[async_trait]
        impl FromNodeNoContext for DummyNoCtx {
            type Error = DummyNoCtxErr;
            async fn from_node(
                _addr: &Addr,
                _node: &Node,
                _cfg: &Config,
            ) -> std::result::Result<Self, DummyNoCtxErr> {
                Ok(DummyNoCtx)
            }
        }

        non_configurable_no_context!(DummyNoCtx, DummyNoCtxErr);

        assert_eq!(
            <DummyNoCtx as crate::context::DefinableNoContext<
                DummyNoCtxErr,
                crate::context::NonConfigurable<DummyNoCtxErr>,
            >>::key(),
            "noop",
        );
        let mut d = DummyNoCtx;
        let cfg = crate::context::NonConfigurable::<DummyNoCtxErr>::default();
        <DummyNoCtx as crate::context::DefinableNoContext<
            DummyNoCtxErr,
            crate::context::NonConfigurable<DummyNoCtxErr>,
        >>::set_config(&mut d, &cfg)
        .unwrap();
    }
}
