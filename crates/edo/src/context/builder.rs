//! Project loading and build orchestration.
//!
//! This module contains [`Project`], which walks a directory tree for
//! `edo.toml` files, deserializes them into a [`Schema`], merges them under
//! their nested namespaces, runs vendor-backed dependency resolution
//! (writing/reading `edo.lock.json` along the way), and finally registers
//! the resulting caches, environments, sources, and transforms with the
//! [`Context`].

use super::Context;
use super::address::Addr;
use super::lock::Lock;
use super::{ContextResult as Result, error};
use crate::context::schema::Schema;
use crate::source::{Dependency, Resolver};
use snafu::{OptionExt, ResultExt};
use std::collections::HashMap;
use std::fs::{File, read_dir};
use std::path::{Path, PathBuf};

/// Intermediate representation of a loaded edo project.
///
/// Owns the merged [`Schema`] (built up from every `edo.toml` discovered
/// under the project root) and the absolute path to that root. Construct
/// it implicitly via [`Project::load`].
pub struct Project {
    project_path: PathBuf,
    schema: Schema,
}

impl Project {
    /// Computes a stable digest over the schema's `requires` table.
    ///
    /// Used to invalidate `edo.lock.json` when dependency declarations
    /// change. Only the requires table participates: changes to plugin
    /// configuration that don't affect resolution shouldn't invalidate
    /// resolved versions.
    fn calculate_digest(&self) -> Result<String> {
        let mut hasher = blake3::Hasher::new();
        for (addr, requirement) in self.schema.requires() {
            hasher.update(addr.to_string().as_bytes());
            hasher.update(requirement.kind.as_bytes());
            hasher.update(requirement.at.to_string().as_bytes());
        }
        let digest = hasher.finalize();
        Ok(base16::encode_lower(digest.as_bytes()))
    }

    /// Loads all `edo.toml` files under `path`, resolves dependencies, and
    /// registers plugins, environments, and transforms with the given
    /// [`Context`].
    pub async fn load<P: AsRef<Path>>(path: P, ctx: &Context, error_on_lock: bool) -> Result<()> {
        let mut schema = Schema::default();
        Self::walk(&Addr::default(), path.as_ref(), &mut schema)?;
        let mut project = Self {
            project_path: path.as_ref().to_path_buf(),
            schema,
        };
        project.build(ctx, error_on_lock).await?;
        Ok(())
    }

    /// Recursively walks `directory`, parsing every `edo.toml` it finds and
    /// merging it into `schema` under the corresponding directory-derived
    /// namespace.
    fn walk(namespace: &Addr, directory: &Path, schema: &mut Schema) -> Result<()> {
        let read = read_dir(directory).context(error::IoSnafu)?;
        for entry in read {
            let entry = entry.context(error::IoSnafu)?;
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|x| x.to_str()) == Some("edo.toml") {
                debug!(component = "project", "loading project file {path:?}");
                let toml_text = std::fs::read_to_string(&path).context(error::IoSnafu)?;
                let mut toml_schema: Schema =
                    toml::from_str(&toml_text).context(error::DeserializeSnafu)?;
                toml_schema.propagate();
                toml_schema.with_namespace(namespace);
                schema.merge(&toml_schema);
            } else if path.is_dir() {
                let dir_name = Addr::parse(path.file_name().and_then(|x| x.to_str()).unwrap())?;
                let addr = namespace.join(&dir_name);
                Self::walk(&addr, &path, schema)?;
            }
        }
        Ok(())
    }

    /// Resolves dependencies, registers plugins/environments/transforms, and
    /// writes the lock file when needed.
    pub async fn build(&mut self, ctx: &Context, error_on_lock: bool) -> Result<()> {
        // Calculate the digest of the project configuration.
        let digest = self.calculate_digest()?;
        ctx.add_config(self.schema.get_config());

        // Resolve all storage backends.
        for (name, element) in self.schema.get_source_caches() {
            let addr = Addr::parse(&format!("//edo-source-cache/{name}"))?;
            ctx.add_cache(&addr, element).await?;
        }
        if let Some(element) = self.schema.get_build_cache() {
            ctx.add_cache(&Addr::parse("//edo-build-cache")?, element)
                .await?;
        }
        if let Some(element) = self.schema.get_output_cache() {
            ctx.add_cache(&Addr::parse("//edo-output-cache")?, element)
                .await?;
        }

        // Check for an existing lockfile.
        let lock_file = self.project_path.join("edo.lock.json");
        if lock_file.exists() {
            let mut file = File::open(&lock_file).context(error::IoSnafu)?;
            let lock: Lock = serde_json::from_reader(&mut file).context(error::SerializeSnafu)?;
            // If digests match, reuse the existing resolution rather than
            // re-running the resolver.
            if lock.digest() == digest {
                info!(target: "project", "no changes detected in project, reusing lock resolution file");
                // Collect first to release the borrow on `self.schema`
                // before mutating it via `add_source`.
                let pending: Vec<_> = self.schema.requires().keys().cloned().collect();
                for addr in pending {
                    let resolved = lock
                        .content()
                        .get(&addr)
                        .context(error::MalformedLockSnafu { addr: addr.clone() })?;
                    self.schema.add_source(&addr, resolved);
                }
            } else if error_on_lock {
                return error::DependencyChangeSnafu {}.fail();
            }
        } else {
            // No lockfile: build a resolver from the registered vendors and
            // resolve every `[requires.*]` entry.
            let mut resolver = Resolver::default();
            let mut vendors = HashMap::new();
            for (addr, element) in self.schema.vendors() {
                let vendor = ctx.add_vendor(element).await?;
                vendors.insert(addr.to_string(), vendor.clone());
                debug!(
                    section = "context",
                    component = "project",
                    "register vendor {addr}"
                );
                resolver.add_vendor(&addr.to_string(), vendor.clone());
            }
            // For every requires entry, build the resolver database and
            // register the dependency.
            let mut need_resolution = Vec::new();
            for (addr, requirement) in self.schema.requires() {
                debug!(
                    section = "context",
                    component = "project",
                    "{addr} needs resolution"
                );
                let dep = Dependency::new(addr, requirement, ctx).await?;
                resolver.build_db(dep.name.as_str()).await?;
                need_resolution.push(dep);
            }

            // resolvo runs synchronously off rayon; offload via spawn_blocking.
            let resolved = tokio::task::spawn_blocking(move || resolver.resolve(need_resolution))
                .await
                .context(error::ResolverJoinSnafu)??;

            let mut lock = Lock::new(digest);

            for (addr, (vendor_name, name, version)) in resolved.iter() {
                debug!(
                    section = "context",
                    component = "project",
                    "resolved {addr} to {name}@{version} from vendor {vendor_name}"
                );
                let vendor = vendors
                    .get(vendor_name)
                    .context(error::MissingVendorSnafu { name: vendor_name })?;
                let resolved = vendor.resolve(name, version).await?;
                lock.content_mut().insert(addr.clone(), resolved.clone());
                self.schema.add_source(addr, &resolved);
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(self.project_path.join("edo.lock.json"))
                .context(error::IoSnafu)?;
            serde_json::to_writer_pretty(&mut file, &lock).context(error::SerializeSnafu)?;
        }

        // Resolve every embedded source map into concrete element references.
        self.schema.resolve_sources()?;

        for (addr, element) in self.schema.environments() {
            debug!(
                section = "context",
                component = "project",
                "adding environment farm {addr}"
            );
            ctx.add_farm(element).await?;
        }

        for (addr, element) in self.schema.transforms() {
            debug!(
                section = "context",
                component = "project",
                "adding transform {addr}"
            );
            ctx.add_transform(element).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the project loader.
    //!
    //! `Project::build` requires a fully-initialized `Context` (including a
    //! global tracing subscriber), so it's covered by the integration tests
    //! in `mod.rs`. Here we focus on the pure pieces: digest stability and
    //! the directory walk.
    use super::*;
    use tempfile::TempDir;

    fn addr(s: &str) -> Addr {
        Addr::parse(s).unwrap()
    }

    fn write_edo_toml(dir: &Path, content: &str) {
        std::fs::write(dir.join("edo.toml"), content).expect("write edo.toml");
    }

    fn empty_project(path: &Path) -> Project {
        Project {
            project_path: path.to_path_buf(),
            schema: Schema::default(),
        }
    }

    /// Builds a `Project` whose schema contains a single `[requires.<addr>]`
    /// entry, by deserializing a tiny TOML document. Going through TOML
    /// avoids depending on the schema's private fields.
    fn project_with_require(path: &Path, addr: Addr, kind: &str, at: &str) -> Project {
        let toml_str = format!("[requires.\"{addr}\"]\nkind = \"{kind}\"\nat = \"{at}\"\n",);
        let schema: Schema = toml::from_str(&toml_str).unwrap();
        assert!(schema.requires().contains_key(&addr));
        Project {
            project_path: path.to_path_buf(),
            schema,
        }
    }

    // ── Project::calculate_digest tests ──────────────────────────────────────

    /// The digest is non-empty and contains only lowercase hex characters.
    #[test]
    fn calculate_digest_is_lowercase_hex() {
        let dir = TempDir::new().unwrap();
        let project = project_with_require(dir.path(), addr("//x"), "image", "=1.0.0");
        let digest = project.calculate_digest().expect("digest ok");
        assert!(!digest.is_empty(), "digest must be non-empty");
        assert!(
            digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "digest must be lowercase hex, got: {digest}",
        );
    }

    /// An empty `requires` table still produces a stable digest (the digest
    /// of the empty BLAKE3 input).
    #[test]
    fn calculate_digest_empty_requires_is_stable() {
        let dir = TempDir::new().unwrap();
        let p = empty_project(dir.path());
        let d1 = p.calculate_digest().unwrap();
        let d2 = p.calculate_digest().unwrap();
        assert_eq!(d1, d2);
    }

    /// The same `requires` content always produces the same digest.
    #[test]
    fn calculate_digest_stable_for_same_input() {
        let dir = TempDir::new().unwrap();
        let p1 = project_with_require(dir.path(), addr("//x"), "image", "=1.0.0");
        let p2 = project_with_require(dir.path(), addr("//x"), "image", "=1.0.0");
        assert_eq!(
            p1.calculate_digest().unwrap(),
            p2.calculate_digest().unwrap()
        );
    }

    /// Different version requirements produce different digests.
    #[test]
    fn calculate_digest_changes_with_version_req() {
        let dir = TempDir::new().unwrap();
        let p1 = project_with_require(dir.path(), addr("//x"), "image", "=1.0.0");
        let p2 = project_with_require(dir.path(), addr("//x"), "image", "=2.0.0");
        assert_ne!(
            p1.calculate_digest().unwrap(),
            p2.calculate_digest().unwrap()
        );
    }

    /// Different kinds produce different digests.
    #[test]
    fn calculate_digest_changes_with_kind() {
        let dir = TempDir::new().unwrap();
        let p1 = project_with_require(dir.path(), addr("//x"), "image", "=1.0.0");
        let p2 = project_with_require(dir.path(), addr("//x"), "git", "=1.0.0");
        assert_ne!(
            p1.calculate_digest().unwrap(),
            p2.calculate_digest().unwrap()
        );
    }

    // ── Project::walk tests ───────────────────────────────────────────────────

    /// `walk` collects elements from the root and a subdirectory, with each
    /// nested file's addresses re-rooted under the directory name.
    #[test]
    fn walk_collects_edo_toml_from_subdirs() {
        let dir = TempDir::new().unwrap();
        let root_content = "[source.\"//a\"]\nkind = \"local\"\npath = \"x\"\n";
        write_edo_toml(dir.path(), root_content);

        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_content = "[source.\"b\"]\nkind = \"local\"\npath = \"y\"\n";
        std::fs::write(sub.join("edo.toml"), sub_content).unwrap();

        let mut schema = Schema::default();
        Project::walk(&Addr::default(), dir.path(), &mut schema).expect("walk ok");

        assert!(
            schema.sources().contains_key(&addr("//a")),
            "schema must contain the absolute root source",
        );
        assert!(
            schema.sources().contains_key(&addr("sub/b")),
            "schema must contain the namespaced subdirectory source",
        );
    }

    /// `walk` on an empty directory succeeds and adds nothing.
    #[test]
    fn walk_empty_directory_is_ok() {
        let dir = TempDir::new().unwrap();
        let mut schema = Schema::default();
        Project::walk(&Addr::default(), dir.path(), &mut schema).expect("walk ok");
        assert!(schema.sources().is_empty(), "schema must be empty");
    }

    /// `walk` ignores non-`edo.toml` files.
    #[test]
    fn walk_skips_non_edo_toml_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("other.toml"),
            "[source.\"//x\"]\nkind = \"local\"\npath=\"y\"\n",
        )
        .unwrap();
        let mut schema = Schema::default();
        Project::walk(&Addr::default(), dir.path(), &mut schema).expect("walk ok");
        assert!(schema.sources().is_empty(), "schema must be empty");
    }

    /// `walk` surfaces malformed TOML as a `Deserialize` error.
    #[test]
    fn walk_malformed_toml_errors() {
        let dir = TempDir::new().unwrap();
        write_edo_toml(dir.path(), "this is = not = valid");
        let mut schema = Schema::default();
        let result = Project::walk(&Addr::default(), dir.path(), &mut schema);
        assert!(
            matches!(result, Err(error::ContextError::Deserialize { .. })),
            "expected Deserialize error, got: {result:?}",
        );
    }
}
