use std::{collections::BTreeMap, fmt, sync::Arc};

use super::{Addr, Config, ContextResult as Result, error};
use async_trait::async_trait;
use parking_lot::RwLock;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use snafu::{OptionExt, ensure};
use starlark::values::Value as StarlarkValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Component {
    StorageBackend,
    Environment,
    Source,
    Transform,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Data {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Require(VersionReq),
    Version(Version),
    List(Vec<Node>),
    Definition {
        id: String,
        kind: String,
        name: String,
        #[serde(flatten)]
        table: BTreeMap<String, Node>,
    },
    Table(BTreeMap<String, Node>),
}

#[async_trait]
pub trait FromNodeNoContext: Sized {
    type Error;

    async fn from_node(
        addr: &Addr,
        node: &Node,
        config: &Config,
    ) -> std::result::Result<Self, Self::Error>;
}

#[async_trait]
pub trait FromNode: Sized {
    type Error;

    async fn from_node(
        addr: &Addr,
        node: &Node,
        ctx: &super::Context,
    ) -> std::result::Result<Self, Self::Error>;
}

impl<'a> TryFrom<&'a barkml::Value> for Node {
    type Error = error::ContextError;

    fn try_from(value: &'a barkml::Value) -> std::result::Result<Self, Self::Error> {
        let data = Data::try_from(value)?;
        Ok(Self {
            data: Arc::new(RwLock::new(data)),
        })
    }
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

impl<'a, 'v> TryFrom<&'a StarlarkValue<'v>> for Node {
    type Error = error::ContextError;

    fn try_from(value: &'a StarlarkValue<'v>) -> std::result::Result<Self, Self::Error> {
        let data = Data::try_from(value)?;
        Ok(Self {
            data: Arc::new(RwLock::new(data)),
        })
    }
}

impl<'a, 'v> TryFrom<&'a StarlarkValue<'v>> for Data {
    type Error = error::ContextError;

    fn try_from(value: &'a StarlarkValue<'v>) -> std::result::Result<Self, Self::Error> {
        let value: JsonValue = value.to_json_value().unwrap();
        Self::try_from(&value)
    }
}

impl<'a> TryFrom<&'a barkml::Value> for Data {
    type Error = error::ContextError;

    fn try_from(value: &'a barkml::Value) -> std::result::Result<Self, Self::Error> {
        if let Some(value) = value.as_bool() {
            Ok(Self::Bool(*value))
        } else if let Some(value) = value.as_int() {
            Ok(Self::Int(*value))
        } else if let Some(value) = value.as_float() {
            Ok(Self::Float(*value))
        } else if let Some(value) = value.as_string().or(value.as_symbol()) {
            Ok(Self::String(value.clone()))
        } else if let Some(value) = value.as_version() {
            Ok(Self::Version(value.clone()))
        } else if let Some(value) = value.as_require() {
            Ok(Self::Require(value.clone()))
        } else if let Some(value) = value.as_array() {
            let mut values = Vec::new();
            for entry in value {
                values.push(Node::try_from(entry)?);
            }
            Ok(Self::List(values))
        } else if let Some(value) = value.as_table() {
            let mut table = BTreeMap::new();
            for (key, value) in value {
                table.insert(key.clone(), Node::try_from(value)?);
            }
            Ok(Self::Table(table))
        } else {
            error::NodeSnafu {}.fail()
        }
    }
}

impl<'a> TryFrom<&'a barkml::Statement> for Node {
    type Error = error::ContextError;

    fn try_from(value: &'a barkml::Statement) -> std::result::Result<Self, Self::Error> {
        let data = Data::try_from(value)?;
        Ok(Self {
            data: Arc::new(RwLock::new(data)),
        })
    }
}

impl<'a> TryFrom<&'a barkml::Statement> for Data {
    type Error = error::ContextError;

    fn try_from(value: &'a barkml::Statement) -> std::result::Result<Self, Self::Error> {
        if let Some((labels, content)) = value.get_labeled() {
            let id = value.id.clone();
            let mut table: BTreeMap<String, Node> = BTreeMap::new();
            for (key, value) in content.iter() {
                if let Some(value) = value.get_value() {
                    table.insert(key.clone(), Node::try_from(value)?);
                } else if value.get_labeled().is_some() {
                    // This is a nested definition
                    table
                        .entry(value.id.clone())
                        .or_insert(Node::new_list(vec![]))
                        .append(Node::try_from(value)?);
                }
            }
            let kind_value = labels.first().context(error::NodeNoKindSnafu)?;
            let kind =
                kind_value
                    .as_string()
                    .or(kind_value.as_symbol())
                    .context(error::FieldSnafu {
                        field: "kind",
                        type_: "string/symbol",
                    })?;
            let name_value = labels.get(1).context(error::NodeNoNameSnafu)?;
            let name =
                name_value
                    .as_string()
                    .or(name_value.as_symbol())
                    .context(error::FieldSnafu {
                        field: "name",
                        type_: "string/symbol",
                    })?;
            Ok(Self::Definition {
                id,
                kind: kind.clone(),
                name: name.clone(),
                table,
            })
        } else {
            error::NodeSnafu {}.fail()
        }
    }
}

macro_rules! as_fn {
    ($fn0: ident, $fn1: ident, $type: ident, $rtype: ty) => {
        pub fn $fn0(value: $rtype) -> Self {
            Self::$type(value)
        }

        pub fn $fn1(&self) -> Option<&$rtype> {
            match self {
                Self::$type(value) => Some(value),
                _ => None,
            }
        }
    };
}

macro_rules! get_field {
    ($gfn: ident, $sfn: ident, $field: ident, $rtype: ty) => {
        pub fn $gfn(&self) -> Option<&$rtype> {
            match self {
                Self::Definition { $field, .. } => Some($field),
                _ => None,
            }
        }

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
    as_fn!(new_bool, as_bool, Bool, bool);
    as_fn!(new_int, as_int, Int, i64);
    as_fn!(new_float, as_float, Float, f64);
    as_fn!(new_string, as_string, String, String);
    as_fn!(new_version, as_version, Version, Version);
    as_fn!(new_require, as_require, Require, VersionReq);
    as_fn!(new_list, as_list, List, Vec<Node>);
    as_fn!(new_table, as_table, Table, BTreeMap<String, Node>);
    get_field!(get_id, set_id, id, String);
    get_field!(get_kind, set_kind, kind, String);
    get_field!(get_name, set_name, name, String);
    get_field!(get_table, set_table, table, BTreeMap<String, Node>);

    pub(crate) fn append(&mut self, item: Node) {
        if let Self::List(items) = self {
            items.push(item)
        }
    }
}

macro_rules! node_field {
    ($gfn: ident, $sfn: ident, $field: ident, $rtype: ty) => {
        pub fn $gfn(&self) -> Option<$rtype> {
            self.data.read().$gfn().cloned()
        }

        pub fn $sfn(&self, value: $rtype) {
            self.data.write().$sfn(value)
        }
    };
}

macro_rules! node_as {
    ($fn0: ident, $fn1: ident, $rtype: ty) => {
        pub fn $fn0(value: $rtype) -> Self {
            Self {
                data: Arc::new(RwLock::new(Data::$fn0(value))),
            }
        }

        pub fn $fn1(&self) -> Option<$rtype> {
            self.data.read().$fn1().cloned()
        }
    };
}

impl Node {
    node_as!(new_bool, as_bool, bool);
    node_as!(new_int, as_int, i64);
    node_as!(new_float, as_float, f64);
    node_as!(new_string, as_string, String);
    node_as!(new_version, as_version, Version);
    node_as!(new_require, as_require, VersionReq);
    node_as!(new_list, as_list, Vec<Node>);
    node_as!(new_table, as_table, BTreeMap<String, Node>);
    node_field!(get_id, set_id, id, String);
    node_field!(get_kind, set_kind, kind, String);
    node_field!(get_name, set_name, name, String);
    node_field!(get_table, set_table, table, BTreeMap<String, Node>);

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

    pub fn data(&self) -> Data {
        self.data.read().clone()
    }

    pub fn set_data(&self, data: &Data) {
        *self.data.write() = data.clone();
    }

    pub(crate) fn append(&self, value: Node) {
        self.data.write().append(value);
    }

    pub fn get(&self, key: &str) -> Option<Node> {
        let read_lock = self.data.read();
        if let Some(table) = read_lock.as_table().or(read_lock.get_table()) {
            table.get(key).cloned()
        } else {
            None
        }
    }
}
