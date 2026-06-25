//! Build context — the central coordinator for an edo build session.
//!
//! The [`Context`] struct ties together configuration, storage, logging,
//! scheduling, plugins, transforms, and environment farms. It is created
//! once per invocation via [`Context::init`] and threaded through the
//! entire build pipeline.
//!
//! Sub-modules provide supporting types:
//! - Addressing — hierarchical [`Addr`] identifiers
//! - Configuration — user-level [`Config`] loaded from `~/.config/edo.toml`
//! - Errors — [`ContextError`] and the [`ContextResult`] alias
//! - Handle — read-only [`Handle`] passed to transforms
//! - Lock — dependency lock file ([`Lock`])
//! - Logging — per-task [`Log`] files and [`LogManager`] tracing setup
//! - Element — [`Element`] plus the [`FromElement`] / [`FromElementNoContext`]
//!   conversion traits
//! - Schema — typed `edo.toml` deserialization ([`Schema`], [`Element`])
//! - Registry — plugin handler registry ([`Registry`], [`Handler`])
//! - Builder — project loading and dependency resolution ([`Project`])

use super::{
    environment::Farm,
    scheduler::Scheduler,
    source::{Source, Vendor},
    transform::Transform,
};
use crate::console::{Console, ConsoleEvent, JsonlSink, SimpleSink};
use crate::storage::{Backend, LocalBackend, Storage};
use dashmap::DashMap;
use serde_json::json;
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
mod element;
pub mod error;
mod handle;
mod lock;
mod log;
mod logmgr;
mod registry;
mod schema;

/// Re-exports [`Addr`] and [`Addressable`].
pub use address::*;
/// Re-exports [`Project`].
pub use builder::*;
/// Re-exports [`Config`].
pub use config::*;
/// Re-exports [`Element`], [`FromElement`], [`FromElementNoContext`],
/// [`SourceDefinition`], and [`SourceMap`].
pub use element::*;
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
/// Re-exports [`Registry`] and the [`Handler`] trait.
pub use registry::*;
/// Re-exports the typed-schema types ([`Schema`], [`Requirement`]).
pub use schema::*;

/// Configuration for the build-event console.
///
/// Determines which sinks the [`Console`] inside [`Context`] is wired with.
/// Mapped from the CLI's `--console-mode` and `--event-log` flags by
/// `crates/cli/src/cmd/mod.rs::create_context`.
#[derive(Clone, Debug)]
pub struct ConsoleConfig {
    /// Renderer mode (`auto`, `full`, `simple`, `none`).
    pub mode: crate::console::ConsoleMode,
    /// Path to the JSONL event log (overwritten each run). `None`
    /// disables the JSONL sink.
    pub event_log: Option<PathBuf>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            mode: crate::console::ConsoleMode::Auto,
            event_log: None,
        }
    }
}

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
    /// Build-event console (typed event bus + sinks)
    console: Console,
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
        console_cfg: ConsoleConfig,
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
        // Wire up the build-event console. The JSONL sink is on by
        // default; disable by passing `event_log = None`.
        let console = Console::new();
        if let Some(jsonl_path) = &console_cfg.event_log {
            match JsonlSink::create(jsonl_path) {
                Ok(sink) => console.add_sink(sink),
                Err(e) => {
                    // Sink failure must never block a build; warn loudly
                    // and continue with the remaining sinks.
                    warn!(
                        subsystem = "console",
                        op = "open-event-log",
                        path = %jsonl_path.display(),
                        "failed to open event log {}: {e}", jsonl_path.display()
                    );
                }
            }
        }
        match crate::console::sinks::resolve_mode(console_cfg.mode) {
            crate::console::ConsoleMode::Simple => {
                console.add_sink(SimpleSink::new());
            }
            crate::console::ConsoleMode::Full => {
                // Spawn the inline ratatui canvas. Default canvas
                // height is 8 rows; the renderer caps the active-task
                // table internally.
                console.install_canvas(8);
            }
            crate::console::ConsoleMode::None | crate::console::ConsoleMode::Auto => {
                // `Auto` is normalised by `resolve_mode`; reaching here
                // means stderr is non-TTY and we want no canvas.
                // `None` means no console output at all.
            }
        }
        // Load the configuration
        let config = Config::load(config).await?;
        // Initialize the storage with the default local cache
        let local_backend_addr = Addr::parse("//edo-local-cache")?;
        let storage = Storage::init(&Backend::new(
            LocalBackend::new(
                &Element::builder()
                    .addr(local_backend_addr.clone())
                    .kind("local")
                    .config([(
                        "path".to_string(),
                        json!(&path.join("storage").to_string_lossy().to_string()),
                    )])
                    .build(),
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
            console,
            storage,
            registry: Registry::default(),
            scheduler: Scheduler::new(&path.join("env"), &config).await?,
            farms: Arc::new(DashMap::new()),
            transforms: Arc::new(DashMap::new()),
        };
        Ok(ctx.clone())
    }

    /// Return the context's project directory
    pub fn project_dir(&self) -> &Path {
        self.project_dir.as_path()
    }

    /// Adds any project found config nodes to the config
    pub fn add_config(&self, config: &BTreeMap<String, serde_json::Value>) {
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
            self.console.clone(),
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

    /// Creates a read-only [`Handle`] snapshot for use by transforms during execution with an argument override
    pub fn get_handle_with_args(&self, args: HashMap<String, String>) -> Handle {
        let mut merged: HashMap<String, String> = self.args.clone();
        merged.extend(args);
        Handle::new(
            self.log.clone(),
            self.console.clone(),
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
            merged,
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

    /// Returns a reference to the build-event console.
    pub fn console(&self) -> &Console {
        &self.console
    }

    /// Convenience: emit a [`ConsoleEvent`] through the build console.
    pub fn emit(&self, event: ConsoleEvent) {
        self.console.emit(event);
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
    pub async fn add_cache(&self, addr: &Addr, element: &Element) -> ContextResult<()> {
        debug!(
            subsystem = "context",
            component = "context",
            op = "register",
            addr = %addr,
            kind = %element.kind,
            "adding a storage backend"
        );
        let backend = if element.kind == "local" || element.kind == "edo:local" {
            Backend::new(LocalBackend::new(element, self.config()).await?)
        } else {
            self.registry().backend(element, self).await?
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
    pub async fn add_transform(&self, element: &Element) -> ContextResult<()> {
        debug!(
            subsystem = "context",
            component = "context",
            op = "register",
            addr = %element.addr,
            kind = %element.kind,
            "adding a transform"
        );
        self.transforms.insert(
            element.addr.clone(),
            self.registry().transform(element, self).await?,
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
    pub async fn add_farm(&self, element: &Element) -> ContextResult<()> {
        debug!(
            subsystem = "context",
            component = "context",
            op = "register",
            addr = %element.addr,
            kind = %element.kind,
            "adding a farm"
        );
        // If we get here use the core plugin
        self.farms.insert(
            element.addr.clone(),
            self.registry().farm(element, self).await?,
        );
        Ok(())
    }

    /// Creates a source fetcher from the given node using the appropriate plugin.
    pub async fn add_source(&self, element: &Element) -> ContextResult<Source> {
        debug!(
            subsystem = "context",
            component = "context",
            op = "register",
            addr = %element.addr,
            kind = %element.kind,
            "adding a source"
        );
        let result = self.registry().source(element, self).await?;
        Ok(result)
    }

    /// Creates a dependency vendor from the given node using the appropriate plugin.
    pub async fn add_vendor(&self, element: &Element) -> ContextResult<Vendor> {
        let result = self.registry().vendor(element, self).await?;
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

        // Provenance: surface farm-setup as its own pre-build phase so
        // the canvas / JSONL log show what's happening between
        // `ProjectLoaded` and `BuildStarted`. Container farms in
        // particular can spend significant time here pulling images and
        // loading them into the runtime; without these events the UI
        // shows a blank screen for that whole window.
        let total = self.farms.len();
        let phase_started = std::time::Instant::now();
        self.emit(ConsoleEvent::EnvSetupStarted { total });

        for entry in self.farms.iter() {
            let addr = entry.key().clone();
            self.emit(ConsoleEvent::EnvSetupFarmStarted { addr: addr.clone() });
            let started = std::time::Instant::now();
            let result = entry
                .setup(&log, self.storage())
                .instrument(info_span!(
                    "env-setup",
                    subsystem = "environment",
                    addr = %addr
                ))
                .await;
            let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            self.emit(ConsoleEvent::EnvSetupFarmFinished {
                addr,
                ok: result.is_ok(),
                elapsed_ms,
            });
            // Emit the phase-end event before propagating the error so
            // sinks see a balanced started/finished pair even on
            // failure. The build won't start in this case.
            if let Err(e) = result {
                let elapsed_ms = phase_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                self.emit(ConsoleEvent::EnvSetupFinished { elapsed_ms });
                return Err(e.into());
            }
        }

        let elapsed_ms = phase_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.emit(ConsoleEvent::EnvSetupFinished { elapsed_ms });
        Ok(())
    }

    /// Sets up environments and executes the build for the given transform address.
    ///
    /// The inline canvas is always shut down before this method returns,
    /// regardless of which phase failed. Without this guarantee an early
    /// `setup_environments` failure would `?`-propagate past the cleanup
    /// and leave the tty in raw mode with a frozen canvas overlay —
    /// the user would see the snafu error chain printed underneath but
    /// have no functioning prompt or shell.
    pub async fn run(&self, addr: &Addr) -> ContextResult<()> {
        let env_setup = self.setup_environments().await;
        let build_result = if env_setup.is_ok() {
            self.scheduler().run(self, addr).await
        } else {
            // Skip scheduling but still tear the canvas down below.
            Ok(())
        };
        // Drain the inline canvas before propagating any error so the
        // user sees the final BuildFinished summary (or the env-setup
        // error chain) on a restored terminal.
        self.console.shutdown().await;
        env_setup?;
        build_result?;
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
            ConsoleConfig::default(),
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
            let element = Element::builder()
                .addr(Addr::parse(addr_str).unwrap())
                .kind("local")
                .config([(
                    "path".to_string(),
                    json!(&&tmp.path().to_string_lossy().to_string()),
                )])
                .build();
            ctx.add_cache(&Addr::parse(addr_str).unwrap(), &element)
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
        // `script` has no plugin-registered provider in a fresh registry,
        // so the registry surfaces a `NoProvider` error before any plugin
        // is invoked.
        let element = Element {
            addr: Addr::parse("//no-provider").unwrap(),
            kind: "script".into(),
            environment: None,
            source: None,
            config: BTreeMap::from([("foo".to_string(), serde_json::Value::String("x".into()))]),
        };
        let err = ctx
            .add_transform(&element)
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
    async fn context_add_farm_unknown_kind_errors() {
        let ctx = ctx_or_skip!();
        // An element with an unrecognised kind surfaces a `NoProvider`
        // error from the registry. (Under the old `Node`-based API a
        // missing `kind` field could not be expressed at all; with the
        // typed schema `kind` is mandatory and serde rejects elements
        // missing it at deserialisation time.)
        let element = Element::builder()
            .addr(Addr::parse("//bad-farm").unwrap())
            .kind("definitely-not-a-real-kind")
            .config(BTreeMap::default())
            .build();
        let err = ctx
            .add_farm(&element)
            .await
            .expect_err("expected NoProvider error");
        assert!(
            matches!(
                err,
                error::ContextError::NoProvider { ref component, .. } if component == "environment"
            ),
            "unexpected error: {err:?}",
        );
        assert!(ctx.get_farm(&Addr::parse("//f").unwrap()).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_missing_kind_in_toml_fails_to_deserialize() {
        // The typed schema requires every element to carry a `kind`. A
        // TOML block without one is a deserialisation error — the
        // strongest replacement for the old runtime `Field { field: "kind" }`
        // check.
        let toml_str = "path = \"x\"\n";
        let result: std::result::Result<Element, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "element without `kind` must fail to deserialise: {result:?}",
        );
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
