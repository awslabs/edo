//! Element-to-typed conversion traits and the source-dependency map.
//!
//! [`Element`] is the shared shape parsed from each `[<section>.<addr>]`
//! block in `edo.toml`. The [`FromElement`] and [`FromElementNoContext`]
//! traits define how typed values (storage backends, sources, transforms,
//! vendors, environment farms) are constructed from raw [`Element`]
//! definitions. [`SourceMap`] models the optional `source = ...` field in
//! its three accepted shapes.

use super::{Addr, Config, ContextResult, error};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use std::collections::BTreeMap;

/// The raw TOML shapes accepted for an element's `source` field.
///
/// All three forms normalize to a `BTreeMap<String, Vec<Addr>>` inside
/// [`SourceMap::Unresolved`].
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum SourceDefinition {
    /// `source = "//addr"` — bound under the `"default"` scope.
    Single(Addr),
    /// `source = ["//a", "//b"]` — all bound under the `"default"` scope.
    List(Vec<Addr>),
    /// `source = { build = ["//a"], runtime = ["//b"] }` — one entry per scope.
    Table(BTreeMap<String, Vec<Addr>>),
}

/// A two-state representation of an element's source dependencies.
///
/// On deserialization the map starts as [`SourceMap::Unresolved`], holding
/// raw addresses. Once [`Schema::resolve_sources`] runs each address is
/// looked up in the schema's source table and the map transitions to
/// [`SourceMap::Resolved`].
#[derive(Serialize, Debug, Clone)]
pub enum SourceMap {
    /// Pre-resolution state: scope name -> list of source addresses.
    Unresolved(BTreeMap<String, Vec<Addr>>),
    /// Post-resolution state: scope name -> list of resolved [`Element`]s.
    Resolved(BTreeMap<String, Vec<Element>>),
}

impl<'de> Deserialize<'de> for SourceMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let def = SourceDefinition::deserialize(deserializer)?;
        Ok(Self::from(def))
    }
}

impl From<SourceDefinition> for SourceMap {
    fn from(value: SourceDefinition) -> Self {
        match value {
            SourceDefinition::Single(addr) => {
                Self::Unresolved(BTreeMap::from([("default".into(), vec![addr])]))
            }
            SourceDefinition::List(list) => {
                Self::Unresolved(BTreeMap::from([("default".into(), list)]))
            }
            SourceDefinition::Table(table) => Self::Unresolved(table),
        }
    }
}

impl SourceMap {
    /// Rewrites every relative address inside an unresolved map so it is
    /// rooted at `namespace`. Absolute addresses (those that already start
    /// with `//`) pass through unchanged because [`Addr::join`] preserves
    /// them. A no-op for [`SourceMap::Resolved`].
    pub fn with_namespace(&mut self, namespace: &Addr) {
        match self {
            Self::Unresolved(table) => {
                *table = table
                    .iter()
                    .map(|(scope, value)| {
                        (
                            scope.clone(),
                            value.iter().map(|addr| namespace.join(addr)).collect(),
                        )
                    })
                    .collect();
            }
            Self::Resolved(_) => {}
        }
    }

    /// Looks up every address in `elements` and transitions this map from
    /// [`SourceMap::Unresolved`] to [`SourceMap::Resolved`]. Returns
    /// [`error::ContextError::MissingSource`] if any address is missing
    /// from `elements`. Already-resolved maps are returned unchanged.
    pub fn resolve(&mut self, elements: &BTreeMap<Addr, Element>) -> ContextResult<()> {
        match self {
            Self::Unresolved(table) => {
                let mut resolution = BTreeMap::new();
                for (scope, value) in table.iter() {
                    let mut entries = Vec::new();
                    for entry in value {
                        let element = elements.get(entry).context(error::MissingSourceSnafu {
                            id: entry.to_string(),
                        })?;
                        entries.push(element.clone());
                    }
                    resolution.insert(scope.clone(), entries);
                }
                *self = Self::Resolved(resolution);
                Ok(())
            }
            Self::Resolved(_) => Ok(()),
        }
    }

    /// Returns the resolved scope -> elements table, or `None` if this map
    /// is still in the [`SourceMap::Unresolved`] state.
    pub fn get_resolved(&self) -> Option<&BTreeMap<String, Vec<Element>>> {
        match self {
            Self::Unresolved(_) => None,
            Self::Resolved(table) => Some(table),
        }
    }
}

/// A single typed plugin definition parsed from `edo.toml`.
///
/// Every `[<section>.<addr>]` block in the project file deserializes into
/// an `Element`. The `kind` field selects the plugin handler from the
/// [`super::Registry`]; `source` (if present) declares input artifacts;
/// any remaining keys flow through the `config` table to the plugin.
#[derive(Serialize, Deserialize, Debug, Clone, bon::Builder)]
pub struct Element {
    /// The element's address in the project tree
    #[serde(skip, default)]
    #[builder(into)]
    pub addr: Addr,
    /// Plugin discriminator (e.g. `"local"`, `"git"`, `"script"`).
    #[builder(into)]
    pub kind: String,
    /// Optional declaration of the environment this element should operate in
    /// (only used by transforms)
    #[builder(into)]
    pub environment: Option<Addr>,
    /// Optional declaration of input source addresses. Shape is one of
    /// the variants of [`SourceDefinition`].
    #[serde(default)]
    #[builder(into)]
    pub source: Option<SourceMap>,
    /// All other top-level keys in the block, kept as raw TOML for the
    /// plugin to interpret.
    #[serde(flatten)]
    #[builder(into)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl Element {
    /// Rewrites every relative address inside this element's [`SourceMap`]
    /// so it is rooted at `namespace`. A no-op when `source` is absent.
    pub fn with_namespace(&mut self, namespace: &Addr) {
        self.addr = namespace.join(&self.addr);
        if let Some(sourcemap) = self.source.as_mut() {
            sourcemap.with_namespace(namespace);
        }
    }

    /// Helper method to deserialize an element's config into a serde type.
    ///
    /// Builds a `serde_json::Value::Object` directly from `self.config` so
    /// the entire config tree is cloned only once, rather than the
    /// double-clone (`to_value` + `from_value`) that `json!(&self.config)`
    /// would perform.
    pub fn get<T>(&self) -> ContextResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let object = serde_json::Value::Object(
            self.config
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        serde_json::from_value(object).context(error::InvalidSnafu {
            kind: self.kind.clone(),
            addr: self.addr.clone(),
        })
    }
}

/// Constructs a typed value from an [`Element`] using only a [`Config`]
/// reference (no full `Context` needed).
#[async_trait]
pub trait FromElementNoContext: Sized {
    /// The error type returned when conversion fails.
    type Error;

    /// Builds `Self` from the given element and configuration.
    async fn new(element: &Element, config: &Config) -> std::result::Result<Self, Self::Error>;
}

/// Constructs a typed value from an [`Element`] using the full
/// [`super::Context`].
#[async_trait]
pub trait FromElement: Sized {
    /// The error type returned when conversion fails.
    type Error;

    /// Builds `Self` from the given element and build context.
    async fn new(element: &Element, ctx: &super::Context)
    -> std::result::Result<Self, Self::Error>;
}
