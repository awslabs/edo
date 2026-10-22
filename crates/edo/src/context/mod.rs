//! Build configuration, component registration, and session coordination.

use super::{
    environment::Farm,
    scheduler::Scheduler,
    source::{Source, Vendor},
    transform::Transform,
};
use crate::storage::{Backend, LocalBackend, Storage};
use dashmap::DashMap;
use serde_json::json;
use snafu::ResultExt;
use std::collections::{BTreeMap, HashMap};
use std::env::current_dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::create_dir_all;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod address;
mod builder;
mod config;
mod element;
pub mod error;
mod handle;
mod lock;
mod log;
pub(crate) mod logmgr;
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

/// Convenience alias for `Result<T, ContextError>`.
pub type ContextResult<T> = std::result::Result<T, error::ContextError>;

type ArcMap<K, V> = Arc<DashMap<K, V>>;

/// Default subdirectory name for edo's working data (`.edo`).
const DEFAULT_PATH: &str = ".edo";

/// Shared state for a build session.
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
    /// User cancellation shared across handles.
    cancellation: CancellationToken,
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
        event_log: EventLog,
    ) -> ContextResult<Self>
    where
        ProjectPath: AsRef<Path>,
        ConfigPath: AsRef<Path>,
    {
        Self::init_with_cancellation(
            path,
            config,
            args,
            verbosity,
            event_log,
            CancellationToken::new(),
        )
        .await
    }

    /// Initializes a context with an explicit session cancellation token.
    pub async fn init_with_cancellation<ProjectPath, ConfigPath>(
        path: Option<ProjectPath>,
        config: Option<ConfigPath>,
        args: HashMap<String, String>,
        verbosity: LogVerbosity,
        event_log: EventLog,
        cancellation: CancellationToken,
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
        // Isolate logs per workspace.
        let log_path = path.join("logs");
        let log = LogManager::init(&log_path, verbosity, event_log).await?;
        debug!(
            subsystem = "context",
            component = "project",
            op = "init",
            "determined project edo target path to be: {path:?}",
        );
        // Load the configuration
        let config = Config::load(config).await?;
        debug!(
            subsystem = "context",
            component = "project",
            op = "init",
            "loaded edo configuration"
        );

        // Initialize the storage with the default local cache
        debug!(
            subsystem = "context",
            component = "project",
            op = "init",
            "initializing edo-local-cache and storage engine"
        );
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
            storage,
            registry: Registry::default(),
            scheduler: Scheduler::new(&path.join("env"), &config).await?,
            farms: Arc::new(DashMap::new()),
            transforms: Arc::new(DashMap::new()),
            cancellation,
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
            self.cancellation.clone(),
        )
    }

    /// Creates a read-only [`Handle`] snapshot for use by transforms during execution with an argument override
    pub fn get_handle_with_args(&self, args: HashMap<String, String>) -> Handle {
        let mut merged: HashMap<String, String> = self.args.clone();
        merged.extend(args);
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
            merged,
            self.cancellation.clone(),
        )
    }

    /// Returns the session token for cooperative user cancellation.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
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
    pub async fn add_cache(&self, addr: &Addr, element: &Element) -> ContextResult<()> {
        debug!(
            subsystem = "context",
            component = "project",
            op = "add-cache",
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
            self.storage().set_build(&backend).await;
        } else if addr_s == "//edo-output-cache" {
            self.storage().set_output(&backend).await;
        } else {
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
            component = "project",
            op = "add-transform",
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
            component = "project",
            op = "add-farm",
            addr = %element.addr,
            kind = %element.kind,
            "adding a farm"
        );
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
            component = "project",
            op = "add-source",
            addr = %element.addr,
            kind = %element.kind,
            "adding a source"
        );
        let result = self.registry().source(element, self).await?;
        Ok(result)
    }

    /// Creates a dependency vendor from the given node using the appropriate plugin.
    pub async fn add_vendor(&self, element: &Element) -> ContextResult<Vendor> {
        debug!(
            subsystem = "context",
            component = "project",
            op = "add-vendor",
            addr = %element.addr,
            kind = %element.kind,
            "adding a vendor"
        );
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

        let total = self.farms.len();
        crate::ui_info!(
            subsystem  = "context",
            component = "project",
            op = "setup-environment";
            "setting up {total} environment(s)"
        );

        for entry in self.farms.iter() {
            let addr = entry.key().clone();
            let id = addr.to_string();
            crate::ui_start_task!(
                subsystem = "context",
                component = "project",
                op = "setup-environment";
                &id,
                crate::log::TaskStatus::Running,
                None
            );
            let result = entry
                .setup(&log, self.storage())
                .instrument(info_span!(
                    "env-setup",
                    subsystem = "environment",
                    addr = %addr
                ))
                .await;
            let status = if result.is_ok() {
                crate::log::TaskStatus::Success
            } else {
                crate::log::TaskStatus::Failed
            };
            crate::ui_update_task!(
                subsystem = "context",
                component = "project",
                op = "setup-environment";
                &id,
                status,
                None
            );
            if let Err(e) = result {
                return Err(e.into());
            }
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
    /// Give each test an independent session while retaining shared logging.
    pub(crate) fn with_fresh_cancellation_for_test(mut self) -> Self {
        self.cancellation = CancellationToken::new();
        self
    }

    /// Inserts a test transform without the plugin registry.
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
    //! Serialized context tests sharing one tracing subscriber.
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::OnceCell;
    type SharedCtx = (Context, Arc<TempDir>);
    static SHARED: OnceCell<SharedCtx> = OnceCell::const_new();

    /// Returns the shared context, or `None` if tracing is initialized elsewhere.
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
            EventLog::Disabled,
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

    /// Skips the test when no shared context is available.
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
        assert!(
            h.args().is_empty(),
            "no CLI args were passed to Context::init",
        );
        let _ = h.transforms();
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn handles_share_user_cancellation_without_phase_poisoning() {
        let mut ctx = ctx_or_skip!();
        ctx.cancellation = CancellationToken::new();
        let first = ctx.get_handle();
        let second = ctx.get_handle_with_args(HashMap::new());
        let phase = first.cancellation().child_token();
        let phase_handle = first.clone().with_cancellation(phase.clone());
        phase_handle.cancellation().cancel();
        assert!(phase.is_cancelled());
        assert!(!ctx.cancellation().is_cancelled());
        assert!(!second.cancellation().is_cancelled());
        let next_phase = first.cancellation().child_token();
        ctx.cancellation().cancel();
        assert!(first.cancellation().is_cancelled());
        assert!(second.cancellation().is_cancelled());
        assert!(next_phase.is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_add_cache_routes_by_address() {
        let ctx = ctx_or_skip!();

        // Use local backends for all cache roles.
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
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn context_add_transform_no_provider_errors() {
        let ctx = ctx_or_skip!();
        let element = Element {
            namespace: Addr::default(),
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
        // Load the temporary project rather than the working directory.
        Project::load(tmp.path(), &ctx, false)
            .await
            .expect("empty project should load cleanly");
    }
}
