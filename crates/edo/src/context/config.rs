//! User-level config loading.
//!
//! [`Config`] loads and queries the user-level configuration file at
//! `~/.config/edo.toml` (or a custom path). It is passed to component
//! constructors (`FromElement::new` / `FromElementNoContext::new`) so they
//! can read user-scoped settings keyed by component kind.

use crate::context::ArcMap;

use super::{ContextResult as Result, error};
use dashmap::DashMap;
use home::home_dir;
use snafu::{OptionExt, ResultExt};
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// User-level configuration loaded from `~/.config/edo.toml` (or a custom path).
#[derive(Clone, Default)]
pub struct Config {
    configs: ArcMap<String, serde_json::Value>,
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
        let configs: BTreeMap<String, serde_json::Value> =
            toml::from_slice(&bytes).context(error::DeserializeSnafu)?;

        Ok(Self {
            configs: Arc::new(DashMap::from_iter(configs)),
        })
    }

    /// Returns the configuration node for the given key, if present.
    pub fn get(&self, name: &str) -> Option<serde_json::Value> {
        self.configs.get(name).map(|x| x.value().clone())
    }

    /// Merges in another set of nodes
    pub fn merge(&self, right: &BTreeMap<String, serde_json::Value>) {
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
        assert_eq!(node.as_str(), Some("bar"));
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
}
