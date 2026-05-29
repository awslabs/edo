//! Dependency lock file representation.
//!
//! A [`Lock`] captures the resolved dependency graph (digest + content map)
//! so that subsequent builds can skip resolution when the project
//! configuration has not changed. It is serialized as `edo.lock.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context::Element;

use super::Addr;

/// A serializable lock file that records the digest of the project
/// configuration and the resolved dependency nodes.
#[derive(Default, Serialize, Deserialize)]
pub struct Lock {
    digest: String,
    #[serde(rename = "refs")]
    content: BTreeMap<Addr, Element>,
}

impl Lock {
    /// Creates a new `Lock` with the given digest and empty content.
    pub fn new(digest: String) -> Self {
        Self {
            digest,
            content: BTreeMap::new(),
        }
    }

    /// Returns the digest string.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Returns a reference to the content map.
    pub fn content(&self) -> &BTreeMap<Addr, Element> {
        &self.content
    }

    /// Returns a mutable reference to the content map.
    pub fn content_mut(&mut self) -> &mut BTreeMap<Addr, Element> {
        &mut self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn addr(s: &str) -> Addr {
        Addr::parse(s).unwrap()
    }

    fn dummy_element(kind: &str) -> Element {
        Element::builder()
            .addr(Addr::parse("//dummy").unwrap())
            .kind(kind)
            .config(BTreeMap::default())
            .build()
    }

    #[test]
    fn new_has_correct_digest_and_empty_content() {
        let lock = Lock::new("digest".to_string());
        assert_eq!(lock.digest(), "digest");
        assert!(lock.content().is_empty());
    }

    #[test]
    fn content_mut_insert_is_reflected_in_content() {
        let mut lock = Lock::new("abc".to_string());
        let a = addr("//proj/node");
        lock.content_mut().insert(a.clone(), dummy_element("local"));
        assert_eq!(lock.content().len(), 1);
        assert!(lock.content().contains_key(&a));
    }

    #[test]
    fn serde_json_round_trip() {
        let mut lock = Lock::new("round-trip".to_string());
        let a = addr("//proj/item");
        lock.content_mut().insert(a.clone(), dummy_element("local"));

        let json = serde_json::to_string(&lock).unwrap();
        let restored: Lock = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.digest(), "round-trip");
        assert!(restored.content().contains_key(&a));
        assert_eq!(restored.content()[&a].kind, "local");
    }

    #[test]
    fn serde_rename_refs_not_content() {
        let mut lock = Lock::new("x".to_string());
        lock.content_mut()
            .insert(addr("//p/q"), dummy_element("local"));

        let json = serde_json::to_string(&lock).unwrap();
        assert!(json.contains("\"refs\":"), "expected \"refs:\" in {json}");
        assert!(
            !json.contains("\"content\":"),
            "unexpected \"content:\" in {json}"
        );
    }
}
