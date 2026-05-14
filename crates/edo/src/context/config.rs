//! Configuration traits and user-level config loading.
//!
//! Provides the [`Definable`] and [`DefinableNoContext`] traits that allow
//! plugins and components to declare a configuration key and accept
//! configuration from either the node definition or the user's global
//! `~/.config/edo.toml`. [`NonConfigurable`] is a zero-cost marker for
//! components that need no configuration. [`Config`] loads and queries the
//! user-level configuration file.

use crate::context::ArcMap;

use super::{Addr, Context, ContextResult as Result, FromNode, FromNodeNoContext, Node, error};
use async_trait::async_trait;
use dashmap::DashMap;
use home::home_dir;
use snafu::{OptionExt, ResultExt};
use std::{collections::BTreeMap, marker::PhantomData, path::Path, sync::Arc};

/// A component that can be configured from a [`Node`] with access to the
/// build [`Context`].
///
/// Implementors declare a [`key`](Definable::key) used to look up
/// configuration in the user's global config file when no inline `config`
/// block is present.
#[async_trait]
pub trait Definable<E, C: FromNode<Error = E> + Send + Default>: FromNode<Error = E> {
    /// Constructs the component from a node, resolving configuration from the
    /// inline `config` block, the global config file, or the default.
    async fn new(addr: &Addr, node: &Node, ctx: &Context) -> std::result::Result<Self, E> {
        let config_key = Self::key();
        let config_node =
            if let Some(cnode) = node.get_table().as_ref().and_then(|x| x.get("config")) {
                Some(C::from_node(addr, cnode, ctx).await?)
            } else if let Some(cnode) = ctx.config().get(config_key) {
                Some(C::from_node(addr, &cnode, ctx).await?)
            } else {
                None
            };
        let mut me = Self::from_node(addr, node, ctx).await?;
        if let Some(config) = config_node.as_ref() {
            me.set_config(config)?;
        } else {
            me.set_config(&C::default())?;
        }
        Ok(me)
    }

    /// Returns the configuration key used to look up settings in the global config.
    fn key() -> &'static str;
    /// Applies the resolved configuration to this component.
    fn set_config(&mut self, config: &C) -> std::result::Result<(), E>;
}

/// Like [`Definable`], but for components that do not need a [`Context`]
/// reference during construction.
#[async_trait]
pub trait DefinableNoContext<E, C: FromNodeNoContext<Error = E> + Send + Default>:
    FromNodeNoContext<Error = E>
{
    /// Constructs the component from a node without a [`Context`], resolving
    /// configuration from the inline block, the global config, or the default.
    async fn new(addr: &Addr, node: &Node, config: &Config) -> std::result::Result<Self, E> {
        let config_key = Self::key();
        let config_node =
            if let Some(cnode) = node.get_table().as_ref().and_then(|x| x.get("config")) {
                Some(C::from_node(addr, cnode, config).await?)
            } else if let Some(cnode) = config.get(config_key) {
                Some(C::from_node(addr, &cnode, config).await?)
            } else {
                None
            };
        let mut me = Self::from_node(addr, node, config).await?;
        if let Some(config) = config_node.as_ref() {
            me.set_config(config)?;
        } else {
            me.set_config(&C::default())?;
        }
        Ok(me)
    }

    /// Returns the configuration key used to look up settings in the global config.
    fn key() -> &'static str;
    /// Applies the resolved configuration to this component.
    fn set_config(&mut self, config: &C) -> std::result::Result<(), E>;
}

/// A zero-size marker type implementing [`FromNode`] and [`FromNodeNoContext`]
/// for components that require no configuration.
pub struct NonConfigurable<E> {
    _data: PhantomData<E>,
}

impl<E> Default for NonConfigurable<E> {
    fn default() -> Self {
        Self { _data: PhantomData }
    }
}

unsafe impl<E> Send for NonConfigurable<E> {}

#[async_trait]
impl<E> FromNode for NonConfigurable<E> {
    type Error = E;

    async fn from_node(
        _addr: &Addr,
        _node: &Node,
        _: &super::Context,
    ) -> std::result::Result<Self, E> {
        Ok(Self { _data: PhantomData })
    }
}

#[async_trait]
impl<E> FromNodeNoContext for NonConfigurable<E> {
    type Error = E;

    async fn from_node(_addr: &Addr, _node: &Node, _: &Config) -> std::result::Result<Self, E> {
        Ok(Self { _data: PhantomData })
    }
}

/// User-level configuration loaded from `~/.config/edo.toml` (or a custom path).
#[derive(Clone, Default)]
pub struct Config {
    configs: ArcMap<String, Node>,
}

impl Config {
    /// Loads user-level configuration from the given path, or from
    /// `~/.config/edo.toml` if no path is provided.
    pub async fn load<P: AsRef<Path>>(path: Option<P>) -> Result<Self> {
        let path = if let Some(path) = path {
            path.as_ref().to_path_buf()
        } else {
            home_dir()
                .context(error::HomeSnafu)?
                .join(".config/edo.toml")
        };
        if !path.exists() {
            return Ok(Self {
                configs: Arc::new(DashMap::new()),
            });
        }
        let bytes = tokio::fs::read(path).await.context(error::IoSnafu)?;
        let configs: BTreeMap<String, Node> =
            toml::from_slice(&bytes).context(error::DeserializeSnafu)?;

        Ok(Self {
            configs: Arc::new(DashMap::from_iter(configs)),
        })
    }

    /// Returns the configuration node for the given key, if present.
    pub fn get(&self, name: &str) -> Option<Node> {
        self.configs.get(name).map(|x| x.value().clone())
    }

    /// Merges in another set of nodes
    pub fn merge(&self, right: &BTreeMap<String, Node>) {
        for (key, value) in right {
            self.configs.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn load_nonexistent_path_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let cfg = Config::load(Some(&path)).await.expect("ok");
        assert!(cfg.get("anything").is_none());
    }

    #[tokio::test]
    async fn load_valid_toml_returns_parsed_nodes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("edo.toml");
        tokio::fs::write(&path, b"foo = \"bar\"\n").await.unwrap();
        let cfg = Config::load(Some(&path)).await.expect("ok");
        let node = cfg.get("foo").expect("foo exists");
        assert_eq!(node.as_string(), Some("bar".to_string()));
    }

    #[tokio::test]
    async fn load_malformed_toml_returns_deserialize_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        tokio::fs::write(&path, b"this = is = not valid toml")
            .await
            .unwrap();
        let result = Config::load(Some(&path)).await;
        let err = result.err().expect("expected error");
        assert!(
            matches!(err, crate::context::ContextError::Deserialize { .. }),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn load_none_path_is_ok() {
        // No assertion on contents — this depends on user's environment.
        // The function falls back to ~/.config/edo.toml; if that does not exist, returns empty.
        let _cfg = Config::load::<&std::path::Path>(None).await.expect("ok");
    }

    #[tokio::test]
    async fn non_configurable_default_and_from_node() {
        // NonConfigurable<E> must be constructible with Default.
        // FromNodeNoContext::from_node always returns Ok for arbitrary input.
        let _n: NonConfigurable<()> = NonConfigurable::default();
        let addr = Addr::parse("//x").unwrap();
        let node = Node::new_bool(true);
        let cfg = Config::load::<&std::path::Path>(None).await.unwrap();
        let res = <NonConfigurable<()> as FromNodeNoContext>::from_node(&addr, &node, &cfg).await;
        assert!(res.is_ok());
    }
}
