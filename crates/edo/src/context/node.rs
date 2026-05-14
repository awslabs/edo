//! Generic data tree used throughout the build graph.
//!
//! [`Node`] is a thread-safe, reference-counted wrapper around [`Data`] that
//! represents any value in an edo configuration — scalars, lists, tables, and
//! full definitions (id + kind + name + table). [`Component`] enumerates the
//! high-level plugin component types. The [`FromNode`] and
//! [`FromNodeNoContext`] traits define how typed values are constructed from
//! raw nodes.

use std::{collections::BTreeMap, fmt, sync::Arc};

use super::{Addr, Config, ContextResult as Result, error};
use async_trait::async_trait;
use parking_lot::RwLock;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use snafu::ensure;

/// Identifies the type of plugin component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Component {
    /// A storage backend component.
    StorageBackend,
    /// A build environment component.
    Environment,
    /// A source fetcher component.
    Source,
    /// A build transform component.
    Transform,
    /// A dependency vendor component.
    Vendor,
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageBackend => f.write_str("storage-backend"),
            Self::Environment => f.write_str("environment"),
            Self::Source => f.write_str("source"),
            Self::Transform => f.write_str("transform"),
            Self::Vendor => f.write_str("vendor"),
        }
    }
}

/// A thread-safe, reference-counted configuration value.
///
/// Wraps a [`Data`] variant behind an `Arc<RwLock<…>>` so it can be shared
/// across async tasks and mutated when dependency resolution fills in
/// resolved data.
#[derive(Clone, Debug)]
pub struct Node {
    data: Arc<RwLock<Data>>,
}

impl Serialize for Node {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let data = self.data.read().clone();
        data.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = Data::deserialize(deserializer)?;
        Ok(Self {
            data: Arc::new(RwLock::new(data)),
        })
    }
}

/// The underlying data variants that a [`Node`] can hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Data {
    /// A boolean value.
    Bool(bool),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit floating-point number.
    Float(f64),
    /// A UTF-8 string.
    String(String),
    /// A semver version requirement (e.g. `>=1.0`).
    Require(VersionReq),
    /// A semver version (e.g. `1.2.3`).
    Version(Version),
    /// An ordered list of child nodes.
    List(Vec<Node>),
    /// A named definition with id, kind, name, and a key-value table.
    Definition {
        /// Unique identifier for this definition.
        id: String,
        /// The kind discriminator (e.g. `"local"`, `"container"`).
        kind: String,
        /// Human-readable name.
        name: String,
        /// Additional key-value pairs.
        #[serde(flatten)]
        table: BTreeMap<String, Node>,
    },
    /// An anonymous key-value table.
    Table(BTreeMap<String, Node>),
}

/// Constructs a typed value from a [`Node`] using only a [`Config`] reference
/// (no full `Context` needed).
#[async_trait]
pub trait FromNodeNoContext: Sized {
    /// The error type returned when conversion fails.
    type Error;

    /// Builds `Self` from the given node and configuration.
    async fn from_node(
        addr: &Addr,
        node: &Node,
        config: &Config,
    ) -> std::result::Result<Self, Self::Error>;
}

/// Constructs a typed value from a [`Node`] using the full [`super::Context`].
#[async_trait]
pub trait FromNode: Sized {
    /// The error type returned when conversion fails.
    type Error;

    /// Builds `Self` from the given node and build context.
    async fn from_node(
        addr: &Addr,
        node: &Node,
        ctx: &super::Context,
    ) -> std::result::Result<Self, Self::Error>;
}

impl<'a> TryFrom<&'a JsonValue> for Node {
    type Error = error::ContextError;

    fn try_from(value: &'a JsonValue) -> std::result::Result<Self, Self::Error> {
        let data = Data::try_from(value)?;
        Ok(Self {
            data: Arc::new(RwLock::new(data)),
        })
    }
}

impl<'a> TryFrom<&'a JsonValue> for Data {
    type Error = error::ContextError;

    fn try_from(value: &'a JsonValue) -> std::result::Result<Self, Self::Error> {
        match value {
            JsonValue::Bool(flag) => Ok(Data::new_bool(*flag)),
            JsonValue::String(string) => Ok(Data::new_string(string.clone())),
            JsonValue::Number(number) => {
                if number.is_f64() {
                    Ok(Data::new_float(number.as_f64().unwrap()))
                } else {
                    Ok(Data::new_int(number.as_i64().unwrap()))
                }
            }
            JsonValue::Array(entries) => {
                let mut values = Vec::new();
                for entry in entries {
                    values.push(Node::try_from(entry)?);
                }
                Ok(Data::new_list(values))
            }
            JsonValue::Object(content) => {
                let mut values = BTreeMap::new();
                for (key, value) in content {
                    values.insert(key.clone(), Node::try_from(value)?);
                }
                Ok(Data::new_table(values))
            }
            _ => error::NodeSnafu {}.fail(),
        }
    }
}

impl<'a> TryFrom<&'a toml::Value> for Node {
    type Error = error::ContextError;

    fn try_from(value: &'a toml::Value) -> std::result::Result<Self, Self::Error> {
        let data = Data::try_from(value)?;
        Ok(Node {
            data: Arc::new(RwLock::new(data)),
        })
    }
}

impl<'a> TryFrom<&'a toml::Value> for Data {
    type Error = error::ContextError;

    fn try_from(value: &'a toml::Value) -> std::result::Result<Self, Self::Error> {
        if let Some(flag) = value.as_bool() {
            Ok(Self::new_bool(flag))
        } else if let Some(string) = value.as_str() {
            // If we have a string we need to try and fail some things
            if string.starts_with(['^', '>', '=', '<', '~'])
                && let Ok(require) = VersionReq::parse(string)
            {
                Ok(Self::new_require(require))
            } else if string.starts_with('v')
                && let Ok(version) = Version::parse(string.strip_prefix('v').unwrap())
            {
                Ok(Self::new_version(version))
            } else {
                Ok(Self::new_string(string.to_string()))
            }
        } else if let Some(float) = value.as_float() {
            Ok(Self::new_float(float))
        } else if let Some(int) = value.as_integer() {
            Ok(Self::new_int(int))
        } else if let Some(items) = value.as_array() {
            let mut array = Vec::new();
            for item in items.iter() {
                array.push(Node::try_from(item)?);
            }
            Ok(Self::new_list(array))
        } else if let Some(items) = value.as_table() {
            let mut table = BTreeMap::new();
            for (key, value) in items.iter() {
                table.insert(key.clone(), Node::try_from(value)?);
            }
            Ok(Self::new_table(table))
        } else {
            error::NodeSnafu {}.fail()
        }
    }
}

macro_rules! as_fn {
    ($fn0: ident, $fn1: ident, $type: ident, $rtype: ty, $doc_new: expr, $doc_as: expr) => {
        #[doc = $doc_new]
        pub fn $fn0(value: $rtype) -> Self {
            Self::$type(value)
        }

        #[doc = $doc_as]
        pub fn $fn1(&self) -> Option<&$rtype> {
            match self {
                Self::$type(value) => Some(value),
                _ => None,
            }
        }
    };
}

macro_rules! get_field {
    ($gfn: ident, $sfn: ident, $field: ident, $rtype: ty, $doc_get: expr, $doc_set: expr) => {
        #[doc = $doc_get]
        pub fn $gfn(&self) -> Option<&$rtype> {
            match self {
                Self::Definition { $field, .. } => Some($field),
                _ => None,
            }
        }

        #[doc = $doc_set]
        pub fn $sfn(&mut self, value: $rtype) {
            match self {
                Self::Definition { $field, .. } => {
                    *$field = value;
                }
                _ => {}
            }
        }
    };
}

impl Data {
    as_fn!(
        new_bool,
        as_bool,
        Bool,
        bool,
        "Creates a new boolean data value.",
        "Returns the boolean value if this is a `Bool` variant."
    );
    as_fn!(
        new_int,
        as_int,
        Int,
        i64,
        "Creates a new integer data value.",
        "Returns the integer value if this is an `Int` variant."
    );
    as_fn!(
        new_float,
        as_float,
        Float,
        f64,
        "Creates a new floating-point data value.",
        "Returns the float value if this is a `Float` variant."
    );
    as_fn!(
        new_string,
        as_string,
        String,
        String,
        "Creates a new string data value.",
        "Returns the string value if this is a `String` variant."
    );
    as_fn!(
        new_version,
        as_version,
        Version,
        Version,
        "Creates a new semver version data value.",
        "Returns the version if this is a `Version` variant."
    );
    as_fn!(
        new_require,
        as_require,
        Require,
        VersionReq,
        "Creates a new version requirement data value.",
        "Returns the version requirement if this is a `Require` variant."
    );
    as_fn!(
        new_list,
        as_list,
        List,
        Vec<Node>,
        "Creates a new list data value.",
        "Returns the list of nodes if this is a `List` variant."
    );
    as_fn!(new_table, as_table, Table, BTreeMap<String, Node>, "Creates a new table data value.", "Returns the table if this is a `Table` variant.");
    get_field!(
        get_id,
        set_id,
        id,
        String,
        "Returns the definition id if this is a `Definition` variant.",
        "Sets the definition id if this is a `Definition` variant."
    );
    get_field!(
        get_kind,
        set_kind,
        kind,
        String,
        "Returns the definition kind if this is a `Definition` variant.",
        "Sets the definition kind if this is a `Definition` variant."
    );
    get_field!(
        get_name,
        set_name,
        name,
        String,
        "Returns the definition name if this is a `Definition` variant.",
        "Sets the definition name if this is a `Definition` variant."
    );
    get_field!(get_table, set_table, table, BTreeMap<String, Node>, "Returns the definition table if this is a `Definition` variant.", "Sets the definition table if this is a `Definition` variant.");

    #[allow(dead_code)]
    pub(crate) fn append(&mut self, item: Node) {
        if let Self::List(items) = self {
            items.push(item)
        }
    }
}

macro_rules! node_field {
    ($gfn: ident, $sfn: ident, $field: ident, $rtype: ty, $doc_get: expr, $doc_set: expr) => {
        #[doc = $doc_get]
        pub fn $gfn(&self) -> Option<$rtype> {
            self.data.read().$gfn().cloned()
        }

        #[doc = $doc_set]
        pub fn $sfn(&self, value: $rtype) {
            self.data.write().$sfn(value)
        }
    };
}

macro_rules! node_as {
    ($fn0: ident, $fn1: ident, $rtype: ty, $doc_new: expr, $doc_as: expr) => {
        #[doc = $doc_new]
        pub fn $fn0(value: $rtype) -> Self {
            Self {
                data: Arc::new(RwLock::new(Data::$fn0(value))),
            }
        }

        #[doc = $doc_as]
        pub fn $fn1(&self) -> Option<$rtype> {
            self.data.read().$fn1().cloned()
        }
    };
}

impl Node {
    node_as!(
        new_bool,
        as_bool,
        bool,
        "Creates a new node holding a boolean value.",
        "Returns the boolean value if the underlying data is a `Bool`."
    );
    node_as!(
        new_int,
        as_int,
        i64,
        "Creates a new node holding an integer value.",
        "Returns the integer value if the underlying data is an `Int`."
    );
    node_as!(
        new_float,
        as_float,
        f64,
        "Creates a new node holding a floating-point value.",
        "Returns the float value if the underlying data is a `Float`."
    );
    node_as!(
        new_string,
        as_string,
        String,
        "Creates a new node holding a string value.",
        "Returns the string value if the underlying data is a `String`."
    );
    node_as!(
        new_version,
        as_version,
        Version,
        "Creates a new node holding a semver version.",
        "Returns the version if the underlying data is a `Version`."
    );
    node_as!(
        new_require,
        as_require,
        VersionReq,
        "Creates a new node holding a version requirement.",
        "Returns the version requirement if the underlying data is a `Require`."
    );
    node_as!(
        new_list,
        as_list,
        Vec<Node>,
        "Creates a new node holding a list of child nodes.",
        "Returns the list of child nodes if the underlying data is a `List`."
    );
    node_as!(new_table, as_table, BTreeMap<String, Node>, "Creates a new node holding a key-value table.", "Returns the table if the underlying data is a `Table`.");
    node_field!(
        get_id,
        set_id,
        id,
        String,
        "Returns the definition id if the underlying data is a `Definition`.",
        "Sets the definition id if the underlying data is a `Definition`."
    );
    node_field!(
        get_kind,
        set_kind,
        kind,
        String,
        "Returns the definition kind if the underlying data is a `Definition`.",
        "Sets the definition kind if the underlying data is a `Definition`."
    );
    node_field!(
        get_name,
        set_name,
        name,
        String,
        "Returns the definition name if the underlying data is a `Definition`.",
        "Sets the definition name if the underlying data is a `Definition`."
    );
    node_field!(get_table, set_table, table, BTreeMap<String, Node>, "Returns the definition table if the underlying data is a `Definition`.", "Sets the definition table if the underlying data is a `Definition`.");

    /// Creates a new node holding a [`Data::Definition`] with the given id, kind, name, and table.
    pub fn new_definition(id: &str, kind: &str, name: &str, table: BTreeMap<String, Node>) -> Self {
        Self {
            data: Arc::new(RwLock::new(Data::Definition {
                id: id.into(),
                kind: kind.into(),
                name: name.into(),
                table,
            })),
        }
    }

    /// Checks that the node's table contains all of the specified keys, returning
    /// an error listing any that are missing.
    pub fn validate_keys(&self, keys: &[&str]) -> Result<()> {
        if let Some(table) = self.as_table().or(self.get_table()) {
            let mut missing = Vec::new();
            for key in keys {
                let key = key.to_string();
                if !table.contains_key(&key) {
                    missing.push(key);
                }
            }
            ensure!(
                missing.is_empty(),
                error::NodeMissingKeysSnafu { keys: missing }
            );
        }
        Ok(())
    }

    /// Returns a clone of the underlying [`Data`] value.
    pub fn data(&self) -> Data {
        self.data.read().clone()
    }

    /// Replaces the underlying [`Data`] value with a clone of the given data.
    pub fn set_data(&self, data: &Data) {
        *self.data.write() = data.clone();
    }

    #[allow(dead_code)]
    pub(crate) fn append(&self, value: Node) {
        self.data.write().append(value);
    }

    /// Looks up a child node by key in the table or definition table.
    pub fn get(&self, key: &str) -> Option<Node> {
        let read_lock = self.data.read();
        if let Some(table) = read_lock.as_table().or(read_lock.get_table()) {
            table.get(key).cloned()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn def(id: &str, kind: &str, name: &str, table: BTreeMap<String, Node>) -> Node {
        Node::new_definition(id, kind, name, table)
    }

    fn table_node(pairs: &[(&str, Node)]) -> Node {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        Node::new_table(m)
    }

    // ── Component::Display ───────────────────────────────────────────────────

    #[test]
    fn component_display_storage_backend() {
        assert_eq!(Component::StorageBackend.to_string(), "storage-backend");
    }

    #[test]
    fn component_display_environment() {
        assert_eq!(Component::Environment.to_string(), "environment");
    }

    #[test]
    fn component_display_source() {
        assert_eq!(Component::Source.to_string(), "source");
    }

    #[test]
    fn component_display_transform() {
        assert_eq!(Component::Transform.to_string(), "transform");
    }

    #[test]
    fn component_display_vendor() {
        assert_eq!(Component::Vendor.to_string(), "vendor");
    }

    // ── Data variant accessors ───────────────────────────────────────────────

    #[test]
    fn data_bool_accessor() {
        let d = Data::new_bool(true);
        assert_eq!(d.as_bool(), Some(&true));
        assert!(d.as_int().is_none());
    }

    #[test]
    fn data_int_accessor() {
        let d = Data::new_int(42);
        assert_eq!(d.as_int(), Some(&42_i64));
        assert!(d.as_bool().is_none());
    }

    #[test]
    fn data_float_accessor() {
        let d = Data::new_float(1.5);
        assert_eq!(d.as_float(), Some(&1.5_f64));
        assert!(d.as_int().is_none());
    }

    #[test]
    fn data_string_accessor() {
        let d = Data::new_string("hello".to_string());
        assert_eq!(d.as_string(), Some(&"hello".to_string()));
        assert!(d.as_bool().is_none());
    }

    #[test]
    fn data_version_accessor() {
        let v = Version::parse("1.2.3").unwrap();
        let d = Data::new_version(v.clone());
        assert_eq!(d.as_version(), Some(&v));
        assert!(d.as_bool().is_none());
    }

    #[test]
    fn data_require_accessor() {
        let r = VersionReq::parse(">=1.0").unwrap();
        let d = Data::new_require(r.clone());
        assert_eq!(d.as_require(), Some(&r));
        assert!(d.as_bool().is_none());
    }

    #[test]
    fn data_list_accessor() {
        let list = vec![Node::new_bool(true), Node::new_int(1)];
        let d = Data::new_list(list);
        assert!(d.as_list().is_some());
        assert_eq!(d.as_list().unwrap().len(), 2);
        assert!(d.as_bool().is_none());
    }

    #[test]
    fn data_table_accessor() {
        let mut m = BTreeMap::new();
        m.insert("x".to_string(), Node::new_int(7));
        let d = Data::new_table(m);
        assert!(d.as_table().is_some());
        assert!(d.as_table().unwrap().contains_key("x"));
        assert!(d.as_bool().is_none());
    }

    // ── Data::get_*/set_* (Definition fields) ────────────────────────────────

    #[test]
    fn data_get_fields_non_definition_returns_none() {
        let d = Data::new_string("x".to_string());
        assert!(d.get_id().is_none());
        assert!(d.get_kind().is_none());
        assert!(d.get_name().is_none());
        assert!(d.get_table().is_none());
    }

    #[test]
    fn data_definition_field_getters() {
        let mut table = BTreeMap::new();
        table.insert("foo".to_string(), Node::new_bool(false));
        let d = Data::Definition {
            id: "my-id".to_string(),
            kind: "my-kind".to_string(),
            name: "my-name".to_string(),
            table,
        };
        assert_eq!(d.get_id(), Some(&"my-id".to_string()));
        assert_eq!(d.get_kind(), Some(&"my-kind".to_string()));
        assert_eq!(d.get_name(), Some(&"my-name".to_string()));
        assert!(d.get_table().unwrap().contains_key("foo"));
    }

    #[test]
    fn data_definition_field_setters() {
        let mut d = Data::Definition {
            id: "old-id".to_string(),
            kind: "old-kind".to_string(),
            name: "old-name".to_string(),
            table: BTreeMap::new(),
        };
        d.set_id("new-id".to_string());
        d.set_kind("new-kind".to_string());
        d.set_name("new-name".to_string());
        let mut new_table = BTreeMap::new();
        new_table.insert("k".to_string(), Node::new_int(1));
        d.set_table(new_table);

        assert_eq!(d.get_id(), Some(&"new-id".to_string()));
        assert_eq!(d.get_kind(), Some(&"new-kind".to_string()));
        assert_eq!(d.get_name(), Some(&"new-name".to_string()));
        assert!(d.get_table().unwrap().contains_key("k"));
    }

    #[test]
    fn data_set_id_on_non_definition_is_noop() {
        let mut d = Data::new_bool(true);
        d.set_id("ignored".to_string()); // must not panic
        assert_eq!(d.as_bool(), Some(&true));
    }

    // ── Data::append ─────────────────────────────────────────────────────────

    #[test]
    fn data_append_pushes_onto_list() {
        let mut d = Data::new_list(vec![]);
        d.append(Node::new_int(1));
        d.append(Node::new_int(2));
        assert_eq!(d.as_list().unwrap().len(), 2);
    }

    #[test]
    fn data_append_noop_on_non_list() {
        let mut d = Data::new_bool(true);
        d.append(Node::new_int(99)); // must not panic
        assert_eq!(d.as_bool(), Some(&true));
    }

    // ── Node accessors ────────────────────────────────────────────────────────

    #[test]
    fn node_bool_roundtrip() {
        let n = Node::new_bool(false);
        assert_eq!(n.as_bool(), Some(false));
        assert!(n.as_int().is_none());
    }

    #[test]
    fn node_int_roundtrip() {
        let n = Node::new_int(-7);
        assert_eq!(n.as_int(), Some(-7_i64));
    }

    #[test]
    fn node_float_roundtrip() {
        let n = Node::new_float(1.5);
        assert_eq!(n.as_float(), Some(1.5_f64));
    }

    #[test]
    fn node_string_roundtrip() {
        let n = Node::new_string("world".to_string());
        assert_eq!(n.as_string(), Some("world".to_string()));
    }

    #[test]
    fn node_version_roundtrip() {
        let v = Version::parse("2.0.0").unwrap();
        let n = Node::new_version(v.clone());
        assert_eq!(n.as_version(), Some(v));
    }

    #[test]
    fn node_require_roundtrip() {
        let r = VersionReq::parse("^1.2").unwrap();
        let n = Node::new_require(r.clone());
        assert_eq!(n.as_require(), Some(r));
    }

    #[test]
    fn node_list_roundtrip() {
        let items = vec![Node::new_int(1), Node::new_int(2)];
        let n = Node::new_list(items);
        assert_eq!(n.as_list().unwrap().len(), 2);
    }

    #[test]
    fn node_table_roundtrip() {
        let n = table_node(&[("a", Node::new_bool(true))]);
        let t = n.as_table().unwrap();
        assert!(t.contains_key("a"));
    }

    // ── Node::set_data ────────────────────────────────────────────────────────

    #[test]
    fn node_set_data_replaces_variant() {
        let n = Node::new_bool(true);
        n.set_data(&Data::new_int(7));
        assert_eq!(n.as_int(), Some(7_i64));
        assert!(n.as_bool().is_none());
    }

    // ── Node::new_definition ─────────────────────────────────────────────────

    #[test]
    fn node_new_definition_getters() {
        let mut t = BTreeMap::new();
        t.insert("entry".to_string(), Node::new_bool(true));
        let n = def("i", "k", "n", t);
        assert_eq!(n.get_id(), Some("i".to_string()));
        assert_eq!(n.get_kind(), Some("k".to_string()));
        assert_eq!(n.get_name(), Some("n".to_string()));
        assert!(n.get_table().unwrap().contains_key("entry"));
    }

    // ── Node::validate_keys ───────────────────────────────────────────────────

    #[test]
    fn validate_keys_ok_on_table() {
        let n = table_node(&[("a", Node::new_int(1)), ("b", Node::new_int(2))]);
        assert!(n.validate_keys(&["a", "b"]).is_ok());
    }

    #[test]
    fn validate_keys_err_reports_missing() {
        let n = table_node(&[("a", Node::new_int(1))]);
        let result = n.validate_keys(&["a", "c", "d"]);
        match result {
            Err(error::ContextError::NodeMissingKeys { keys }) => {
                assert_eq!(keys, vec!["c".to_string(), "d".to_string()]);
            }
            other => panic!("expected NodeMissingKeys, got {other:?}"),
        }
    }

    #[test]
    fn validate_keys_scalar_node_is_ok() {
        // Non-table/definition variant: no table to check → always Ok
        let n = Node::new_bool(true);
        assert!(n.validate_keys(&["anything"]).is_ok());
    }

    #[test]
    fn validate_keys_definition_with_key_present() {
        let mut t = BTreeMap::new();
        t.insert("x".to_string(), Node::new_int(0));
        let n = def("id", "k", "nm", t);
        assert!(n.validate_keys(&["x"]).is_ok());
    }

    // ── Node::get ─────────────────────────────────────────────────────────────

    #[test]
    fn node_get_from_definition() {
        let mut t = BTreeMap::new();
        t.insert("mykey".to_string(), Node::new_int(42));
        let n = def("i", "k", "n", t);
        assert!(n.get("mykey").is_some());
        assert!(n.get("missing").is_none());
    }

    #[test]
    fn node_get_from_table() {
        let n = table_node(&[("k", Node::new_bool(false))]);
        assert!(n.get("k").is_some());
        assert!(n.get("missing").is_none());
    }

    #[test]
    fn node_get_from_scalar_returns_none() {
        let n = Node::new_bool(true);
        assert!(n.get("anything").is_none());
    }

    // ── TryFrom<&toml::Value> ─────────────────────────────────────────────────

    #[test]
    fn toml_bool() {
        let v: toml::Value = toml::from_str("b = true").unwrap();
        let n = Node::try_from(v.get("b").unwrap()).unwrap();
        assert_eq!(n.as_bool(), Some(true));
    }

    #[test]
    fn toml_int() {
        let v: toml::Value = toml::from_str("i = 42").unwrap();
        let n = Node::try_from(v.get("i").unwrap()).unwrap();
        assert_eq!(n.as_int(), Some(42_i64));
    }

    #[test]
    fn toml_float() {
        let v: toml::Value = toml::from_str("f = 3.5").unwrap();
        let n = Node::try_from(v.get("f").unwrap()).unwrap();
        assert_eq!(n.as_float(), Some(3.5_f64));
    }

    #[test]
    fn toml_plain_string() {
        // "hello" cannot be parsed as VersionReq or Version → stored as String
        let v: toml::Value = toml::from_str(r#"s = "hello""#).unwrap();
        let n = Node::try_from(v.get("s").unwrap()).unwrap();
        assert_eq!(n.as_string(), Some("hello".to_string()));
    }

    #[test]
    fn toml_string_that_parses_as_version_req() {
        // "1.2.3" is a valid VersionReq (semver parses it as >=1.2.3, <2); the
        // TryFrom implementation tries VersionReq first, so this becomes Require.
        let v: toml::Value = toml::from_str(r#"s = "^1.2.3""#).unwrap();
        let n = Node::try_from(v.get("s").unwrap()).unwrap();
        // VersionReq is tried before Version: assert Require (not Version)
        assert!(
            n.as_require().is_some(),
            "expected Require variant for '1.2.3'"
        );
    }

    #[test]
    fn toml_version_req_string() {
        let v: toml::Value = toml::from_str(r#"r = ">=1.0""#).unwrap();
        let n = Node::try_from(v.get("r").unwrap()).unwrap();
        assert!(n.as_require().is_some());
    }

    #[test]
    fn toml_array() {
        let v: toml::Value = toml::from_str("arr = [1, 2, 3]").unwrap();
        let n = Node::try_from(v.get("arr").unwrap()).unwrap();
        assert_eq!(n.as_list().unwrap().len(), 3);
    }

    #[test]
    fn toml_nested_table() {
        let src = "[nested]\ninner = \"x\"\n";
        let v: toml::Value = toml::from_str(src).unwrap();
        let n = Node::try_from(v.get("nested").unwrap()).unwrap();
        assert!(n.get("inner").is_some());
    }

    #[test]
    fn toml_datetime_returns_error() {
        // Parse via a TOML document — toml::value::Datetime::from_str is not
        // stable public API in all versions; use document parsing instead.
        let v: toml::Value = toml::from_str("d = 1979-05-27").unwrap();
        let d = v.get("d").unwrap();
        let result = Node::try_from(d);
        assert!(
            matches!(result, Err(error::ContextError::Node)),
            "expected ContextError::Node for Datetime, got {result:?}"
        );
    }

    // ── TryFrom<&serde_json::Value> ───────────────────────────────────────────

    #[test]
    fn json_bool() {
        let v = serde_json::Value::Bool(true);
        let n = Node::try_from(&v).unwrap();
        assert_eq!(n.as_bool(), Some(true));
    }

    #[test]
    fn json_int() {
        let v = serde_json::json!(42);
        let n = Node::try_from(&v).unwrap();
        assert_eq!(n.as_int(), Some(42_i64));
    }

    #[test]
    fn json_float() {
        let v = serde_json::json!(3.5);
        let n = Node::try_from(&v).unwrap();
        assert_eq!(n.as_float(), Some(3.5_f64));
    }

    #[test]
    fn json_string() {
        let v = serde_json::json!("hi");
        let n = Node::try_from(&v).unwrap();
        assert_eq!(n.as_string(), Some("hi".to_string()));
    }

    #[test]
    fn json_array() {
        let v = serde_json::json!([1, 2]);
        let n = Node::try_from(&v).unwrap();
        assert_eq!(n.as_list().unwrap().len(), 2);
    }

    #[test]
    fn json_object() {
        let v = serde_json::json!({"k": "v"});
        let n = Node::try_from(&v).unwrap();
        assert!(n.get("k").is_some());
    }

    #[test]
    fn json_null_returns_error() {
        let v = serde_json::Value::Null;
        let result = Node::try_from(&v);
        assert!(
            matches!(result, Err(error::ContextError::Node)),
            "expected ContextError::Node for Null, got {result:?}"
        );
    }

    // ── Serde round-trips ─────────────────────────────────────────────────────

    #[test]
    fn serde_definition_roundtrip() {
        let mut t = BTreeMap::new();
        t.insert("other".to_string(), Node::new_string("x".to_string()));
        let n = def("i", "k", "n", t);

        let json = serde_json::to_string(&n).unwrap();
        // Because of #[serde(untagged)] + #[serde(flatten)], the JSON is a
        // flat object: {"id":"i","kind":"k","name":"n","other":"x"}
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get_id(), Some("i".to_string()));
        assert_eq!(back.get_kind(), Some("k".to_string()));
        assert_eq!(back.get_name(), Some("n".to_string()));
    }

    #[test]
    fn serde_table_roundtrip() {
        let n = table_node(&[("alpha", Node::new_int(1)), ("beta", Node::new_bool(true))]);
        let json = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert!(back.get("alpha").is_some());
        assert!(back.get("beta").is_some());
    }
}
