use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Node;

use super::Addr;

#[derive(Default, Serialize, Deserialize)]
pub struct Lock {
    pub digest: String,
    #[serde(rename = "refs")]
    pub content: BTreeMap<Addr, Node>,
}
