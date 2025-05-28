use super::ContextResult as Result;
use serde::{Deserialize, Serialize};
use std::fmt;

pub trait Addressable {
    fn addr(&self) -> &Addr;
    fn name(&self) -> &String;
    fn kind(&self) -> &String;
}

#[derive(Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr(Vec<String>);

impl<'de> Deserialize<'de> for Addr {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Addr::parse(s.as_str()).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Addr {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = self.to_string();
        serializer.serialize_str(s.as_str())
    }
}

impl Addr {
    pub fn parse(input: &str) -> Result<Self> {
        let segment = input.strip_prefix("//").unwrap_or(input);
        Ok(Self(segment.split("/").map(|x| x.to_string()).collect()))
    }

    pub fn join(&self, name: &str) -> Self {
        let mut content = self.0.clone();
        content.push(name.to_string());
        Self(content)
    }

    pub fn parent(&self) -> Option<Addr> {
        if self.0.len() == 1 {
            None
        } else {
            let mut me = self.0.clone();
            me.pop();
            Some(Addr(me))
        }
    }

    pub fn to_id(&self) -> String {
        self.0.join("/")
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("//{}", self.0.join("/")))
    }
}
