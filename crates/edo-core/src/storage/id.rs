use super::error;
use derive_builder::Builder;
use semver::Version;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::{fmt, str::FromStr};

const UNSUPPORTED_CHARS: &[char] = &['@', ':', '.', '-', '/'];
const UNSUPPORTED_PREFIX: &[&str] = &["http://", "https://"];

/// Represents the name of the artifact, artifact names cannot
/// have certain characters in them (@, :, ., -, /) so all of these
/// are replaced with '_'
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct Name(String);

impl Name {
    /// Parse a string into a valid artifact name replacing
    /// all unsupported characters and strings with '_'
    fn parse(value: &str) -> Self {
        let mut value = value.to_string();
        for pattern in UNSUPPORTED_PREFIX {
            value = value
                .strip_prefix(pattern)
                .unwrap_or(value.as_str())
                .to_string();
        }
        for pattern in UNSUPPORTED_CHARS {
            value = value.replace(*pattern, "_");
        }
        Self(value.trim_start_matches('_').to_string())
    }
}

impl<'a> From<&'a str> for Name {
    fn from(value: &'a str) -> Self {
        Self::parse(value)
    }
}

impl From<String> for Name {
    fn from(value: String) -> Self {
        Self::parse(value.as_str())
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// Represents the unique id for an artifact, it optionally can contain a
/// secondary name called the package name, along with an optional version.
/// All ids contain a blake3 digest
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Builder)]
#[builder(setter(into))]
pub struct Id {
    name: Name,
    #[builder(setter(into), default)]
    package: Option<Name>,
    #[builder(setter(into), default)]
    version: Option<Version>,
    #[builder(setter(into), default)]
    arch: Option<String>,
    digest: String,
}

impl Id {
    pub fn name(&self) -> String {
        self.name.clone().to_string()
    }

    pub fn package(&self) -> Option<String> {
        self.package.clone().map(|x| x.to_string())
    }

    pub fn digest(&self) -> &String {
        &self.digest
    }

    pub fn arch(&self) -> Option<String> {
        self.arch.clone()
    }

    pub fn version(&self) -> Option<Version> {
        self.version.clone()
    }

    pub fn set_digest(&mut self, digest: &str) {
        self.digest = digest.to_string();
    }

    pub fn set_version(&mut self, version: &Version) {
        self.version = Some(version.clone());
    }

    pub fn clear_version(&mut self) {
        self.version = None;
    }

    /// The prefix is everything without the digest and can be used
    /// to identify multiple versions of an artifact from a transform
    pub fn prefix(&self) -> String {
        let mut prefix = String::default();
        if let Some(package) = self.package() {
            prefix += package.as_str();
            prefix += "+";
        }
        prefix += self.name().as_str();
        if let Some(version) = self.version() {
            prefix += "-";
            prefix += version.to_string().as_str();
        }
        if let Some(arch) = self.arch() {
            prefix += ".";
            prefix += arch.as_str();
        }
        prefix
    }
}

impl FromStr for Id {
    type Err = super::error::StorageError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (package, s) = if s.contains('+') {
            let (left, right) = s.split_once('+').unwrap();
            (Some(left.into()), right)
        } else {
            (None, s)
        };
        let segments = s.split("-").collect::<Vec<_>>();

        if segments.len() == 2 {
            let (name, arch) = if segments[0].contains(".") {
                segments[0]
                    .split_once(".")
                    .map(|(x, y)| (x, Some(y)))
                    .unwrap()
            } else {
                (segments[0], None)
            };
            Ok(Self {
                name: name.into(),
                package,
                version: None,
                arch: arch.map(|x| x.into()),
                digest: segments[1].into(),
            })
        } else if segments.len() == 3 {
            let (version, arch) = if segments[1].contains(".") {
                segments[0]
                    .split_once(".")
                    .map(|(x, y)| (x, Some(y)))
                    .unwrap()
            } else {
                (segments[0], None)
            };
            Ok(Self {
                name: segments[0].into(),
                package,
                version: Some(Version::parse(version).context(error::SemverSnafu)?),
                arch: arch.map(|x| x.into()),
                digest: segments[1].into(),
            })
        } else {
            error::IdSnafu {
                reason: format!("'{}' is not a valid artifact id", s),
            }
            .fail()
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(package) = self.package() {
            f.write_str(package.as_str())?;
            f.write_str("+")?;
        }
        f.write_str(self.name().as_str())?;
        if let Some(version) = self.version() {
            f.write_str("-")?;
            f.write_str(version.to_string().as_str())?;
        }
        if let Some(arch) = self.arch() {
            f.write_str(".")?;
            f.write_str(arch.as_str())?;
        }
        f.write_str("-")?;
        f.write_str(self.digest())
    }
}

impl Serialize for Id {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        Self::from_str(string.as_str()).map_err(serde::de::Error::custom)
    }
}
