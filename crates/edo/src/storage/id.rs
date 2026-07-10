use super::error;
use bon::Builder;
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use std::{fmt, str::FromStr, sync::LazyLock};

const UNSUPPORTED_CHARS: &[char] = &['@', ':', '.', '-', '/'];
const UNSUPPORTED_PREFIX: &[&str] = &["http://", "https://"];
static ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^
        (?P<name>[A-Za-z0-9_]+)
        (?:\.(?P<arch>[A-Za-z0-9_]+))?
        (?:@(?P<version>
            (?:0|[1-9]\d*)                                    # major
            \.(?:0|[1-9]\d*)                                  # .minor
            \.(?:0|[1-9]\d*)                                  # .patch
            (?:-                                              # -prerelease
              (?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)
              (?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*
            )?
            (?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?          # +build
        ))?
        :(?P<digest>[0-9a-f]{64})
        $
    ",
    )
    .unwrap()
});

/// The human-readable name portion of an artifact [`Id`].
///
/// Certain characters (`@`, `:`, `.`, `-`, `/`) and URL prefixes are replaced
/// with underscores to ensure filesystem and registry compatibility.
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

/// The unique identifier for an artifact in storage.
///
/// Composed of a [`Name`], an optional package name, an optional semver
/// version, an optional architecture tag, and a SHA256 content digest.
/// Serializes to the format `<name>[.arch][@<version>]:<digest>`.
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Builder)]
pub struct Id {
    #[builder(into)]
    name: Name,
    #[builder(into)]
    version: Option<Version>,
    #[builder(into)]
    arch: Option<String>,
    digest: String,
}

impl Id {
    /// Return the artifact name as a string.
    pub fn name(&self) -> String {
        self.name.clone().to_string()
    }

    /// Return a reference to the SHA256 hex digest.
    pub fn digest(&self) -> &String {
        &self.digest
    }

    /// Return the optional architecture tag.
    pub fn arch(&self) -> Option<String> {
        self.arch.clone()
    }

    /// Return the optional semver version.
    pub fn version(&self) -> Option<Version> {
        self.version.clone()
    }

    /// Replace the digest with a new value.
    pub fn set_digest(&mut self, digest: &str) {
        self.digest = digest.to_string();
    }

    /// Set the semver version.
    pub fn set_version(&mut self, version: &Version) {
        self.version = Some(version.clone());
    }

    /// Remove the version component from this id.
    pub fn clear_version(&mut self) {
        self.version = None;
    }

    /// The prefix is everything without the digest and can be used
    /// to identify multiple versions of an artifact from a transform
    pub fn prefix(&self) -> String {
        let mut prefix = String::default();
        prefix += self.name().as_str();
        if let Some(arch) = self.arch() {
            prefix += ".";
            prefix += arch.as_str();
        }
        if let Some(version) = self.version() {
            prefix += "@";
            prefix += version.to_string().as_str();
        }
        prefix
    }
}

impl FromStr for Id {
    type Err = super::error::StorageError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let caps = ID_REGEX.captures(s).context(error::IdSnafu {
            reason: format!("'{s}' is not a valid artifact id"),
        })?;

        // Required groups: unwrap is safe because the regex matched.
        let name = caps.name("name").unwrap().as_str();
        let digest = caps.name("digest").unwrap().as_str();

        // Optional groups: `.name()` returns Option<Match<'_>>.
        let arch = caps.name("arch").map(|m| m.as_str().to_string());
        let version = caps
            .name("version")
            .map(|m| Version::parse(m.as_str()))
            .transpose()
            .context(error::SemverSnafu)?;

        Ok(Self {
            name: name.into(),
            version,
            arch,
            digest: digest.to_string(),
        })
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name().as_str())?;
        if let Some(arch) = self.arch() {
            f.write_str(".")?;
            f.write_str(arch.as_str())?;
        }
        if let Some(version) = self.version() {
            f.write_str("@")?;
            f.write_str(version.to_string().as_str())?;
        }
        f.write_str(":")?;
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
