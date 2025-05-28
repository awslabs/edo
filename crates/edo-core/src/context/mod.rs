use std::{
    collections::{BTreeMap, HashMap},
    env::current_dir,
    path::Path,
    sync::Arc,
};

use crate::{
    std::{core_plugin, environment::LocalFarm, storage::LocalBackend},
    storage::{Backend, Storage},
};

use super::{
    environment::Farm,
    plugin::Plugin,
    scheduler::Scheduler,
    source::{Source, Vendor},
    transform::Transform,
};

mod address;
mod builder;
mod config;
pub mod error;
mod handle;
mod lock;
mod log;
mod logmgr;
mod node;
mod starlark;
pub use address::*;
pub use builder::*;
pub use config::*;
use dashmap::DashMap;
pub use error::ContextError;
pub use handle::*;
pub use lock::*;
pub use log::*;
pub use logmgr::*;
pub use node::*;

use snafu::{OptionExt, ResultExt};
use tokio::fs::create_dir_all;
use tracing::Instrument;

pub type ContextResult<T> = std::result::Result<T, error::ContextError>;

type ArcMap<K, V> = Arc<DashMap<K, V>>;
const DEFAULT_PATH: &str = ".edo";

#[derive(Clone)]
pub struct Context {
    /// Loaded Shared Configuration
    config: Config,
    /// Storage Manager
    storage: Storage,
    /// Log Manager
    log: LogManager,
    /// Execution Scheduler
    scheduler: Scheduler,
    /// Loaded Plugins
    plugins: ArcMap<Addr, Plugin>,
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
    pub async fn init<ProjectPath, ConfigPath>(
        path: Option<ProjectPath>,
        config: Option<ConfigPath>,
        error_on_lock: bool,
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
        let storage = Storage::init(&Backend::from_impl(
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
            config: config.clone(),
            args,
            log: log.clone(),
            storage,
            scheduler: Scheduler::new(&path.join("env"), &config).await?,
            farms: Arc::new(DashMap::new()),
            plugins: Arc::new(DashMap::new()),
            transforms: Arc::new(DashMap::new()),
        };
        ctx.farms.insert(
            Addr::parse("//default")?,
            Farm::from_impl(LocalFarm::default()),
        );
        // We now can load the project information
        Project::load(&project_dir, &ctx, error_on_lock).await?;
        Ok(ctx.clone())
    }

    pub fn get_handle(&self) -> Handle {
        Handle {
            log: self.log.clone(),
            storage: self.storage.clone(),
            transforms: self
                .transforms
                .iter()
                .map(|x| (x.key().clone(), x.value().clone()))
                .collect(),
            farms: self
                .farms
                .iter()
                .map(|x| (x.key().clone(), x.value().clone()))
                .collect(),
            args: self.args.clone(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn log(&self) -> &LogManager {
        &self.log
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    pub fn print_transforms(&self) {
        for addr in self.transforms.iter() {
            println!("{}", addr.key());
        }
    }

    pub fn get_plugin(&self, addr: &Addr) -> Option<Plugin> {
        self.plugins.get(addr).map(|x| x.value().clone())
    }

    pub async fn add_plugin(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        // Plugins cannot add other plugins so this is a discrete switch operation
        debug!(
            section = "context",
            component = "context",
            "adding a plugin {addr}"
        );
        let plugin = Plugin::new(addr, node, self).await?;
        let log = self.log.create("init").await?;
        log.set_subject(&addr.to_string());
        plugin.fetch(&log, self.storage()).await?;
        plugin.setup(&log, self.storage()).await?;
        self.plugins.insert(addr.clone(), plugin);
        Ok(())
    }

    pub fn get_transform(&self, addr: &Addr) -> Option<Transform> {
        self.transforms.get(addr).map(|x| x.value().clone())
    }

    pub async fn add_cache(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a storage backend {addr}"
        );
        if let Some((plugin, kind)) = node.get_kind().and_then(|x| {
            x.split_once(':')
                .map(|y| (y.0.to_string(), y.1.to_string()))
        }) {
            let node = node.clone();
            node.set_kind(kind);
            let plugin_addr = Addr::parse(plugin.as_str())?;
            // If the plugin name is edo it is explicity referring to the core
            // plugin
            if plugin == "edo" {
                let backend = core_plugin().create_storage(addr, &node, self).await?;
                // Now we check the addr if it matches our special addresses we handle them
                // otherwise we add it as a source cache
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
                return Ok(());
            }
            let plugin = self
                .plugins
                .get(&plugin_addr)
                .context(error::NoPluginSnafu {
                    addr: plugin_addr.clone(),
                })?;
            let backend = plugin.create_storage(addr, &node, self).await?;
            // Now we check the addr if it matches our special addresses we handle them
            // otherwise we add it as a source cache
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
            return Ok(());
        }
        // If we get here use the core plugin
        // Now we check the addr if it matches our special addresses we handle them
        // otherwise we add it as a source cache
        let backend = core_plugin().create_storage(addr, node, self).await?;
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

    pub async fn add_transform(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a transform {addr}"
        );
        if let Some((plugin, kind)) = node.get_kind().and_then(|x| {
            x.split_once(':')
                .map(|y| (y.0.to_string(), y.1.to_string()))
        }) {
            let node = node.clone();
            node.set_kind(kind);
            let plugin_addr = Addr::parse(plugin.as_str())?;
            // If the plugin name is edo it is explicity referring to the core
            // plugin
            if plugin == "edo" {
                self.transforms.insert(
                    addr.clone(),
                    core_plugin().create_transform(addr, &node, self).await?,
                );
                return Ok(());
            }
            let plugin = self
                .plugins
                .get(&plugin_addr)
                .context(error::NoPluginSnafu {
                    addr: plugin_addr.clone(),
                })?;
            self.transforms.insert(
                addr.clone(),
                plugin.create_transform(addr, &node, self).await?,
            );
            return Ok(());
        }
        // If we get here use the core plugin
        self.transforms.insert(
            addr.clone(),
            core_plugin().create_transform(addr, node, self).await?,
        );
        Ok(())
    }

    pub async fn prune(&self) -> ContextResult<()> {
        let handle = self.get_handle();
        for transform in self.transforms.iter() {
            let id = transform.get_unique_id(&handle).await?;
            self.storage().prune_local(&id).await?;
        }
        Ok(())
    }

    pub fn get_farm(&self, addr: &Addr) -> Option<Farm> {
        self.farms.get(addr).map(|x| x.value().clone())
    }

    pub async fn add_farm(&self, addr: &Addr, node: &Node) -> ContextResult<()> {
        debug!(
            section = "context",
            component = "context",
            "adding a farm {addr}"
        );
        if let Some((plugin, kind)) = node.get_kind().and_then(|x| {
            x.split_once(':')
                .map(|y| (y.0.to_string(), y.1.to_string()))
        }) {
            let node = node.clone();
            node.set_kind(kind);
            let plugin_addr = Addr::parse(plugin.as_str())?;
            // If the plugin name is edo it is explicity referring to the core
            // plugin
            if plugin == "edo" {
                self.farms.insert(
                    addr.clone(),
                    core_plugin().create_farm(addr, &node, self).await?,
                );
                return Ok(());
            }
            let plugin = self
                .plugins
                .get(&plugin_addr)
                .context(error::NoPluginSnafu {
                    addr: plugin_addr.clone(),
                })?;
            self.farms
                .insert(addr.clone(), plugin.create_farm(addr, &node, self).await?);
            return Ok(());
        }
        // If we get here use the core plugin
        self.farms.insert(
            addr.clone(),
            core_plugin().create_farm(addr, node, self).await?,
        );
        Ok(())
    }

    pub async fn add_source(&self, addr: &Addr, node: &Node) -> ContextResult<Source> {
        debug!(
            section = "context",
            component = "context",
            "adding a source {addr}"
        );
        if let Some((plugin, kind)) = node.get_kind().and_then(|x| {
            x.split_once(':')
                .map(|y| (y.0.to_string(), y.1.to_string()))
        }) {
            let node = node.clone();
            node.set_kind(kind);
            let plugin_addr = Addr::parse(plugin.as_str())?;
            // If the plugin name is edo it is explicity referring to the core
            // plugin
            if plugin == "edo" {
                let result = core_plugin().create_source(addr, &node, self).await?;
                return Ok(result);
            }
            let plugin = self
                .plugins
                .get(&plugin_addr)
                .context(error::NoPluginSnafu {
                    addr: plugin_addr.clone(),
                })?;
            let result = plugin.create_source(addr, &node, self).await?;
            return Ok(result);
        }
        // If we get here use the core plugin
        let result = core_plugin().create_source(addr, node, self).await?;
        Ok(result)
    }

    pub async fn add_vendor(&self, addr: &Addr, node: &Node) -> ContextResult<Vendor> {
        if let Some((plugin, kind)) = node.get_kind().and_then(|x| {
            x.split_once(':')
                .map(|y| (y.0.to_string(), y.1.to_string()))
        }) {
            let node = node.clone();
            node.set_kind(kind);
            let plugin_addr = Addr::parse(plugin.as_str())?;
            // If the plugin name is edo it is explicity referring to the core
            // plugin
            if plugin == "edo" {
                let result = core_plugin().create_vendor(addr, &node, self).await?;
                return Ok(result);
            }
            let plugin = self
                .plugins
                .get(&plugin_addr)
                .context(error::NoPluginSnafu {
                    addr: plugin_addr.clone(),
                })?;
            let result = plugin.create_vendor(addr, &node, self).await?;
            return Ok(result);
        }
        // If we get here use the core plugin
        let result = core_plugin().create_vendor(addr, node, self).await?;
        Ok(result)
    }

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

    pub async fn run(&self, addr: &Addr) -> ContextResult<()> {
        self.setup_environments().await?;
        self.scheduler().run(self, addr).await?;
        Ok(())
    }
}
