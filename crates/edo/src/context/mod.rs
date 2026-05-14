//! Build context — the central coordinator for an edo build session.
//!
//! The [`Context`] struct ties together configuration, storage, logging,
//! scheduling, plugins, transforms, and environment farms. It is created
//! once per invocation via [`Context::init`] and threaded through the
//! entire build pipeline.
//!
//! Sub-modules provide supporting types:
//! - Addressing — hierarchical [`Addr`] identifiers
//! - Configuration — user-level [`Config`] and the [`Definable`] traits
//! - Errors — [`ContextError`] and the [`ContextResult`] alias
//! - Handle — read-only [`Handle`] passed to transforms
//! - Lock — dependency lock file ([`Lock`])
//! - Logging — per-task [`Log`] files and [`LogManager`] tracing setup
//! - Node — generic data tree ([`Node`], [`Data`], [`Component`])
//! - Schema — TOML schema deserialization
//! - Builder — project loading and dependency resolution ([`Project`])

use super::{
    environment::Farm,
    scheduler::Scheduler,
    source::{Source, Vendor},
    transform::Transform,
};
use crate::context::registry::Registry;
use crate::storage::{Backend, LocalBackend, Storage};
use dashmap::DashMap;
use snafu::ResultExt;
use std::collections::{BTreeMap, HashMap};
use std::env::current_dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::create_dir_all;
use tracing::Instrument;

mod address;
mod builder;
mod config;
pub mod error;
mod handle;
mod lock;
mod log;
mod logmgr;
mod node;
mod registry;
mod schema;

/// Re-exports [`Addr`] and [`Addressable`].
pub use address::*;
/// Re-exports [`Project`] and the `non_configurable` macros.
pub use builder::*;
/// Re-exports [`Config`], [`Definable`], [`DefinableNoContext`], and [`NonConfigurable`].
pub use config::*;
/// Re-exports [`ContextError`] at the module level.
pub use error::ContextError;
/// Re-exports [`Handle`].
pub use handle::*;
/// Re-exports [`Lock`].
pub use lock::*;
/// Re-exports [`Log`].
pub use log::*;
/// Re-exports [`LogManager`], [`LogVerbosity`], and logging helpers.
pub use logmgr::*;
/// Re-exports [`Node`], [`Data`], [`Component`], [`FromNode`], and [`FromNodeNoContext`].
pub use node::*;

/// Convenience alias for `Result<T, ContextError>`.
pub type ContextResult<T> = std::result::Result<T, error::ContextError>;

type ArcMap<K, V> = Arc<DashMap<K, V>>;

/// Default subdirectory name for edo's working data (`.edo`).
const DEFAULT_PATH: &str = ".edo";

/// Central coordinator for an edo build session.
///
/// Holds references to configuration, storage, logging, scheduling, and all
/// registered plugins, transforms, and environment farms. Created once via
/// [`Context::init`] and shared (via `Clone`) throughout the build.
#[derive(Clone)]
pub struct Context {
    /// Project directory
    project_dir: PathBuf,
    /// Loaded Shared Configuration
    config: Config,
    /// Storage Manager
    storage: Storage,
    /// Log Manager
    log: LogManager,
    /// Execution Scheduler
    scheduler: Scheduler,
    /// Registry of implemented components
    registry: Registry,
    /// Registered Transforms
    transforms: ArcMap<Addr, Transform>,
    /// Registered Farms
    farms: ArcMap<Addr, Farm>,
    /// Command Line Arguments
    args: HashMap<String, String>,
}

unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    /// Initializes a new build context, setting up logging, configuration,
    /// storage, and the execution scheduler.
    pub async fn init<ProjectPath, ConfigPath>(
        path: Option<ProjectPath>,
        config: Option<ConfigPath>,
        args: HashMap<String, String>,
        verbosity: LogVerbosity,
    ) -> ContextResult<Self>
    where
        ProjectPath: AsRef<Path>,
        ConfigPath: AsRef<Path>,
    {
        let project_dir = current_dir().context(error::IoSnafu)?;
        let path = if let Some(path) = path.as_ref() {
            path.as_ref().to_path_buf()
        } else {
            project_dir.join(DEFAULT_PATH)
        };
        if !path.exists() {
            create_dir_all(&path).await.context(error::IoSnafu)?;
        }
        // Logs should be in a project specific folder, so they
        // do not clash with other project workspaces.
        let log_path = path.join("logs");
        let log = LogManager::init(&log_path, verbosity).await?;
        // Load the configuration
        let config = Config::load(config).await?;
        // Initialize the storage with the default local cache
        let storage = Storage::init(&Backend::new(
            LocalBackend::new(
                &Addr::parse("//edo-local-cache")?,
                &Node::new_definition(
                    "storage",
                    "local",
                    "edo-local-cache",
                    BTreeMap::from([(
                        "path".to_string(),
                        Node::new_string(path.join("storage").to_string_lossy().to_string()),
                    )]),
                ),
                &config,
            )
            .await?,
        ))
        .await?;

        // Create the initial context
        let ctx = Context {
            project_dir: project_dir.clone(),
            config: config.clone(),
            args,
            log: log.clone(),
            storage,
            registry: Registry::default(),
            scheduler: Scheduler::new(&path.join("env"), &config).await?,
            farms: Arc::new(DashMap::new()),
            transforms: Arc::new(DashMap::new()),
        };
        Ok(ctx.clone())
    }

    /// Adds any project found config nodes to the config
    pub fn add_config(&self, config: &BTreeMap<String, Node>) {
        self.config.merge(config);
    }

    /// Loads the project from the current directory, resolving dependencies
    /// and registering all components.
    pub async fn load_project(&self, error_on_lock: bool) -> ContextResult<()> {
        Project::load(&self.project_dir, self, error_on_lock).await?;
        Ok(())
    }

    /// Returns the registry you can add new implementations to
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Creates a read-only [`Handle`] snapshot for use by transforms during execution.
    pub fn get_handle(&self) -> Handle {
        Handle::new(
            self.log.clone(),
            self.config.clone(),
            self.storage.clone(),
            self.transforms
                .iter()
                .map(|x| (x.key().clone(), x.value().clone()))
                .collect(),
            self.farms
                .iter()
                .map(|x| (x.key().clone(), x.value().clone()))
                .collect(),
            self.args.clone(),
        )
    }

    /// Returns a reference to the loaded configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns a reference to the storage manager.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Returns a reference to the log manager.
    pub fn log(&self) -> &LogManager {
        &self.log
    }

    /// Returns a reference to the execution scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Prints all registered transform addresses to stdout.
    pub fn print_transforms(&self) {
        for addr in self.transforms.iter() {
            println!("{}", addr.key());
        }
    }

    /// Returns the transform registered at the given address, if any.
    pub fn get_transform(&self, addr: &Addr) -> Option<Transform> {
        self.transforms.get(addr).map(|x| x.value().clone())
    }

    /// Registers a storage cache backend, routing it to build, output, or source cache
    /// based on the address.
    pub async fn add_cache(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a storage backend {addr}"
        );
        let kind = node.get_kind().unwrap();
        let backend = if kind == "local" || kind == "edo:local" {
            Backend::new(LocalBackend::new(addr, node, self.config()).await?)
        } else {
            self.registry().backend(addr, node, self).await?
        };
        let addr_s = addr.to_string();
        if addr_s == "//edo-build-cache" {
            // This is a build cache so add it
            self.storage().set_build(&backend).await;
        } else if addr_s == "//edo-output-cache" {
            // This is an output cache so add it
            self.storage().set_output(&backend).await;
        } else {
            // This is a source cache
            self.storage()
                .add_source_cache(addr_s.as_str(), &backend)
                .await;
        }
        Ok(())
    }

    /// Creates and registers a transform from the given node using the appropriate plugin.
    pub async fn add_transform(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a transform {addr}"
        );
        self.transforms.insert(
            addr.clone(),
            self.registry().transform(addr, node, self).await?,
        );
        Ok(())
    }

    /// Removes stale local storage entries for all registered transforms.
    pub async fn prune(&self) -> ContextResult<()> {
        let handle = self.get_handle();
        for transform in self.transforms.iter() {
            let id = transform.get_unique_id(&handle).await?;
            self.storage().prune_local(&id).await?;
        }
        Ok(())
    }

    /// Returns the environment farm registered at the given address, if any.
    pub fn get_farm(&self, addr: &Addr) -> Option<Farm> {
        self.farms.get(addr).map(|x| x.value().clone())
    }

    /// Creates and registers an environment farm from the given node using the appropriate plugin.
    pub async fn add_farm(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a farm {addr}"
        );
        // If we get here use the core plugin
        self.farms
            .insert(addr.clone(), self.registry().farm(addr, node, self).await?);
        Ok(())
    }

    /// Creates a source fetcher from the given node using the appropriate plugin.
    pub async fn add_source(&self, addr: &Addr, node: &Node) -> ContextResult<Source> {
        debug!(
            section = "context",
            component = "context",
            "adding a source {addr}"
        );
        let result = self.registry().source(addr, node, self).await?;
        Ok(result)
    }

    /// Creates a dependency vendor from the given node using the appropriate plugin.
    pub async fn add_vendor(&self, addr: &Addr, node: &Node) -> ContextResult<Vendor> {
        let result = self.registry().vendor(addr, node, self).await?;
        Ok(result)
    }

    /// Returns a reference to the command-line arguments map.
    pub fn args(&self) -> &HashMap<String, String> {
        &self.args
    }

    async fn setup_environments(&self) -> ContextResult<()> {
        // Run the initial setup for environments
        let log = self.log.create("setup").await?;
        log.set_subject("environment-setup");
        for entry in self.farms.iter() {
            entry
                .setup(&log, self.storage())
                .instrument(info_span!(
                    target: "context",
                    "setting up environment",
                    addr = entry.key().to_string()
                ))
                .await?;
        }
        Ok(())
    }

    /// Sets up environments and executes the build for the given transform address.
    pub async fn run(&self, addr: &Addr) -> ContextResult<()> {
        self.setup_environments().await?;
        self.scheduler().run(self, addr).await?;
        Ok(())
    }
}

#[cfg(test)]
impl Context {
    /// Test-only: insert a [`Transform`] directly, bypassing the plugin registry.
    ///
    /// Exists because `add_transform` funnels through the registry which is
    /// only populated by `edo-core::register_core`. The `edo` crate cannot
    /// depend on `edo-core` (cycle), so scheduler tests instead construct
    /// mock transforms and inject them here.
    pub(crate) fn insert_transform_for_test(&self, addr: &Addr, transform: Transform) {
        self.transforms.insert(addr.clone(), transform);
    }

    /// Test-only: insert a [`Farm`] directly, bypassing the plugin registry.
    pub(crate) fn insert_farm_for_test(&self, addr: &Addr, farm: Farm) {
        self.farms.insert(addr.clone(), farm);
    }
}

#[cfg(test)]
mod tests {
    //! Integration tests for `Context`.
    //!
    //! `Context::init` installs a global `tracing` subscriber via
    //! `LogManager::init`. That subscriber can only be installed once per
    //! process, so these tests:
    //!
    //! 1. Share a single `Context` via a `OnceCell` (`shared_context`).
    //! 2. Serialize on the `log_manager` tag (shared with any future tests
    //!    in `log.rs` / `logmgr.rs` that also initialize the subscriber).
    //! 3. Treat `ContextError::Log` as a soft skip — if another test in the
    //!    same binary installed the subscriber first and we raced past the
    //!    `OnceCell`, we cannot recover a `Context`, so we skip gracefully.
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::OnceCell;
    type SharedCtx = (Context, Arc<TempDir>);
    static SHARED: OnceCell<SharedCtx> = OnceCell::const_new();

    /// Returns the shared `Context` if available.
    ///
    /// Returns `None` only when `Context::init` fails with `ContextError::Log`
    /// because a sibling test (in `log.rs`/`logmgr.rs`) already installed the
    /// global subscriber. In that case each caller should return early.
    async fn try_shared_context() -> Option<Context> {
        if let Some((ctx, _)) = SHARED.get() {
            return Some(ctx.clone());
        }
        let dir = TempDir::new().expect("create tempdir");
        match Context::init::<&std::path::Path, &std::path::Path>(
            Some(dir.path()),
            None,
            HashMap::new(),
            LogVerbosity::Info,
        )
        .await
        {
            Ok(ctx) => {
                // First writer wins; if another task raced us, accept theirs.
                let _ = SHARED.set((ctx.clone(), Arc::new(dir)));
                Some(SHARED.get().map(|(c, _)| c.clone()).unwrap_or(ctx))
            }
            Err(error::ContextError::Log { .. }) => None,
            Err(e) => panic!("unexpected Context::init error: {e}"),
        }
    }

    /// Convenience that either yields a `Context` or executes `return` via the
    /// caller (through `let ... else`).
    macro_rules! ctx_or_skip {
        () => {
            match try_shared_context().await {
                Some(c) => c,
                None => {
                    eprintln!(
                        "skip: global tracing subscriber already initialized \
                         by a sibling test in this binary"
                    );
                    return;
                }
            }
        };
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_init_smoke() {
        let ctx = ctx_or_skip!();
        // Accessors must not panic and must return references.
        let _ = ctx.config();
        let _ = ctx.storage();
        let _ = ctx.log();
        let _ = ctx.scheduler();
        let _ = ctx.args();
        let _ = ctx.registry();
        // Unregistered lookups return None.
        assert!(
            ctx.get_transform(&Addr::parse("//x").unwrap()).is_none(),
            "no transforms registered yet",
        );
        assert!(
            ctx.get_farm(&Addr::parse("//x").unwrap()).is_none(),
            "no farms registered yet",
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_get_handle_returns_snapshot() {
        let ctx = ctx_or_skip!();
        let h = ctx.get_handle();
        // The shared context may have had transforms/farms registered by
        // a previous test; the snapshot should, however, never be missing
        // the args map.
        assert!(
            h.args().is_empty(),
            "no CLI args were passed to Context::init",
        );
        // `transforms()` is just a `HashMap` borrow — we only assert access.
        let _ = h.transforms();
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_add_cache_routes_by_address() {
        let ctx = ctx_or_skip!();

        // All three routes use the built-in `local` backend path, which is
        // handled directly (not via the plugin registry).
        for addr_str in ["//edo-build-cache", "//edo-output-cache", "//some-src"] {
            let tmp = TempDir::new().unwrap();
            let mut table = BTreeMap::new();
            table.insert(
                "path".to_string(),
                Node::new_string(tmp.path().to_string_lossy().to_string()),
            );
            let node = Node::new_definition("storage", "local", "c", table);
            ctx.add_cache(&Addr::parse(addr_str).unwrap(), &node)
                .await
                .unwrap_or_else(|e| panic!("add_cache {addr_str} failed: {e}"));
        }
        // Storage does not publicly expose slot inspection; reaching this
        // point without panic or error is the assertion.
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_add_transform_no_provider_errors() {
        let ctx = ctx_or_skip!();
        let mut table = BTreeMap::new();
        table.insert("foo".to_string(), Node::new_string("x".to_string()));
        // `script` has no plugin-registered provider in a fresh registry.
        let node = Node::new_definition("transform", "script", "t", table);
        let err = ctx
            .add_transform(&Addr::parse("//t").unwrap(), &node)
            .await
            .expect_err("expected NoProvider error");
        assert!(
            matches!(
                err,
                error::ContextError::NoProvider { ref component, ref kind }
                if component == "transform" && kind == "script"
            ),
            "unexpected error: {err:?}",
        );
        // Ensure nothing was inserted on failure.
        assert!(ctx.get_transform(&Addr::parse("//t").unwrap()).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_add_farm_missing_kind_field_error() {
        let ctx = ctx_or_skip!();
        // A plain Table (not a Definition) — `get_kind()` returns None, so
        // the registry surfaces a `Field { field: "kind", .. }` error.
        let node = Node::new_table(BTreeMap::new());
        let err = ctx
            .add_farm(&Addr::parse("//f").unwrap(), &node)
            .await
            .expect_err("expected Field error");
        assert!(
            matches!(
                err,
                error::ContextError::Field { ref field, .. } if field == "kind"
            ),
            "unexpected error: {err:?}",
        );
        assert!(ctx.get_farm(&Addr::parse("//f").unwrap()).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_print_transforms_smoke() {
        let ctx = ctx_or_skip!();
        // Must not panic even when the transforms map is empty.
        ctx.print_transforms();
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_load_empty_project_ok() {
        let ctx = ctx_or_skip!();
        let tmp = TempDir::new().unwrap();
        // Bypass `self.project_dir` (bound to the CWD at Context::init time)
        // by calling `Project::load` directly with our empty directory.
        Project::load(tmp.path(), &ctx, false)
            .await
            .expect("empty project should load cleanly");
        // An empty project still loads successfully; we do not assert on the
        // presence of a lockfile since `Project::load` may write one
        // unconditionally. Reaching this point is the assertion.
    }
}
