use super::{StorageResult, error, id::Id};
use derive_builder::Builder;
use ocilot::models::Platform;
use regex::Regex;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

const ARTIFACT_SCHEMA_VERSION: &str = "v1";

/// Denotes the use of any compression algorithm on a layer
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum Compression {
    #[serde(rename = ".zst")]
    Zstd,
    #[serde(rename = ".gz", alias = ".gzip", alias = ".gzip2")]
    Gzip,
    #[serde(rename = ".bz2", alias = ".bzip2", alias = ".bzip")]
    Bzip2,
    #[serde(rename = ".lz4", alias = ".lzma")]
    Lz,
    #[serde(rename = ".xz")]
    Xz,
    #[serde(other, rename = "")]
    None,
}

fn split_by(input: &str, pattern: &Regex) -> String {
    if let Some(entry) = pattern.find(input) {
        input.split_at(entry.start()).0.to_string()
    } else {
        input.to_string()
    }
}

impl Compression {
    pub fn detect(input: &str) -> StorageResult<(String, Compression)> {
        let zstd = Regex::new(r"[\.\+]{1}zst$").context(error::RegexSnafu)?;
        if zstd.is_match(input) {
            return Ok((split_by(input, &zstd), Compression::Zstd));
        }
        let gzip = Regex::new(r"[\.\+]{1}(gz|gzip2|gzip)$").context(error::RegexSnafu)?;
        if gzip.is_match(input) {
            return Ok((split_by(input, &gzip), Compression::Gzip));
        }
        let bzip = Regex::new(r"[\.\+]{1}(bz2|bzip2|bzip)$").context(error::RegexSnafu)?;
        if bzip.is_match(input) {
            return Ok((split_by(input, &bzip), Compression::Bzip2));
        }
        let lz = Regex::new(r"[\.\+]{1}(lz4|lzma)$").context(error::RegexSnafu)?;
        if lz.is_match(input) {
            return Ok((split_by(input, &lz), Compression::Lz));
        }
        let xz = Regex::new(r"[\.\+]{1}xz$").context(error::RegexSnafu)?;
        if xz.is_match(input) {
            return Ok((split_by(input, &xz), Compression::Xz));
        }
        Ok((input.to_string(), Compression::None))
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Zstd => ".zst",
            Self::Gzip => ".gz",
            Self::Bzip2 => ".bz2",
            Self::Lz => ".lz4",
            Self::Xz => ".xz",
            Self::None => "",
        })
    }
}

/// Denotes the content of a layer
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum MediaType {
    #[default]
    Manifest,
    File(Compression),
    Tar(Compression),
    Oci(Compression),
    Image(Compression),
    Zip(Compression),
    Custom(String, Compression),
}

impl MediaType {
    pub fn is_compressed(&self) -> bool {
        match self {
            Self::Manifest => false,
            Self::File(comp)
            | Self::Tar(comp)
            | Self::Oci(comp)
            | Self::Image(comp)
            | Self::Zip(comp)
            | Self::Custom(_, comp) => match comp {
                Compression::None => false,
                _ => true,
            },
        }
    }

    pub fn set_compression(&mut self, compression: Compression) {
        match self {
            Self::File(comp)
            | Self::Tar(comp)
            | Self::Oci(comp)
            | Self::Image(comp)
            | Self::Zip(comp)
            | Self::Custom(_, comp) => {
                *comp = compression;
            }
            _ => {}
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.manifest"
            )),
            Self::File(compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.file{compression}"
            )),
            Self::Tar(compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.tar{compression}",
            )),
            Self::Zip(compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.zip{compression}",
            )),
            Self::Oci(compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.oci{compression}",
            )),
            Self::Image(compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.image{compression}",
            )),
            Self::Custom(name, compression) => f.write_fmt(format_args!(
                "vnd.edo.artifact.{ARTIFACT_SCHEMA_VERSION}.{name}{compression}"
            )),
        }
    }
}

impl FromStr for MediaType {
    type Err = error::StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (mut string, compression) = Compression::detect(s)?;
        if !string.starts_with("vnd.edo.artifact") {
            return error::InvalidMediaTypeSnafu { value: s }.fail();
        }
        string = string.strip_prefix("vnd.edo.artifact").unwrap().to_string();
        // We can do portability when we bump version
        let schema_version = format!(".{ARTIFACT_SCHEMA_VERSION}.");
        if !string.starts_with(schema_version.as_str()) {
            return error::SchemaSnafu { value: s }.fail();
        }
        string = string
            .strip_prefix(schema_version.as_str())
            .unwrap()
            .to_string();
        match string.as_str() {
            "manifest" => Ok(MediaType::Manifest),
            "file" => Ok(MediaType::File(compression)),
            "tar" => Ok(MediaType::Tar(compression)),
            "zip" => Ok(MediaType::Zip(compression)),
            "oci" => Ok(MediaType::Oci(compression)),
            "image" => Ok(MediaType::Image(compression)),
            value => Ok(MediaType::Custom(value.to_string(), compression)),
        }
    }
}

impl Serialize for MediaType {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'de> Deserialize<'de> for MediaType {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        Self::from_str(string.as_str()).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// Type alias for any custom metadata stored in an artifact's manifest
pub type Metadata = serde_json::Value;
/// Type alias for versioned requirements
pub type Requires = BTreeMap<String, BTreeMap<String, VersionReq>>;

#[derive(Serialize, Deserialize, Clone, Debug, Builder)]
#[builder(setter(into))]
pub struct Config {
    id: Id,
    #[builder(setter(into), default)]
    provides: BTreeSet<String>,
    #[builder(setter(into), default)]
    requires: Requires,
    #[builder(setter(into), default)]
    metadata: Metadata,
}

macro_rules! handle {
    ($fn0: ident, $fn1: ident, $field: ident, $type: ty) => {
        pub fn $fn0(&self) -> &$type {
            &self.$field
        }

        pub fn $fn1(&mut self) -> &mut $type {
            &mut self.$field
        }
    };
}

impl Config {
    handle!(id, id_mut, id, Id);
    handle!(metadata, metadata_mut, metadata, Metadata);
    handle!(requires, requires_mut, requires, Requires);
    handle!(provides, provides_mut, provides, BTreeSet<String>);
}

#[derive(Debug, Clone)]
pub struct LayerDigest(String);

impl LayerDigest {
    pub fn digest(&self) -> String {
        self.0.clone()
    }
}

impl<'a> From<&'a str> for LayerDigest {
    fn from(value: &'a str) -> Self {
        Self(value.strip_prefix("blake3:").unwrap_or(value).to_string())
    }
}

impl From<String> for LayerDigest {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl Serialize for LayerDigest {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("blake3:{}", self.0))
    }
}

impl<'de> Deserialize<'de> for LayerDigest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let str = String::deserialize(deserializer)?;
        if str.starts_with("blake3:") {
            Ok(Self(str.strip_prefix("blake3:").unwrap().to_string()))
        } else {
            Err(serde::de::Error::custom(
                "not a valid artifact layer digest",
            ))
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct Layer {
    media_type: MediaType,
    digest: LayerDigest,
    size: usize,
    #[builder(setter(into), default)]
    platform: Option<Platform>,
}

impl Layer {
    handle!(media_type, media_type_mut, media_type, MediaType);
    handle!(digest, digest_mut, digest, LayerDigest);
    handle!(size, size_mut, size, usize);
    handle!(platform, platform_mut, platform, Option<Platform>);
}

/// An artifact is used to store any data in-flight or final. All artifacts are stored and represented
/// as an OCI Artifact. How the artifact is stored is up to the storage implementation. For example, a local cache
/// could actually store all blobs associated with all artifacts in the same blob folder and only have different manifest
/// files. This structure usually acts as a fully opened handle to an artifact, and actually contains the manifest
/// data.
#[derive(Serialize, Deserialize, Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct Artifact {
    #[builder(setter(into), default)]
    media_type: MediaType,
    config: Config,
    #[builder(setter(into), default)]
    layers: Vec<Layer>,
}

impl Artifact {
    handle!(config, config_mut, config, Config);
    handle!(media_type, media_type_mut, media_type, MediaType);
    handle!(layers, layers_mut, layers, Vec<Layer>);
}
