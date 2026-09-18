use base64::{
    Engine,
    engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT},
};
use nutype::nutype;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256, Sha512};
use snafu::{OptionExt, ResultExt};
use std::{fmt, path::PathBuf};

use super::error::{self, StorageError};

#[nutype(
    sanitize(trim, lowercase),
    validate(not_empty, regex = r"^[0-9a-fA-F]+$"),
    derive(Debug, PartialEq, Eq, Deserialize, Serialize)
)]
struct Base16(String);

#[nutype(
    sanitize(trim),
    validate(
        not_empty,
        regex = r"^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2,3}=?{0,2})?$"
    ),
    derive(Debug, PartialEq, Eq, Deserialize, Serialize)
)]
struct Base64Standard(String);

#[nutype(
    sanitize(trim),
    validate(
        not_empty,
        regex = r"^(?:[A-Za-z0-9\-_]{4})*(?:[A-Za-z0-9\-_]{2,3}=?{0,2})?$"
    ),
    derive(Debug, PartialEq, Eq, Deserialize, Serialize)
)]
struct Base64URLSafe(String);

fn deserialize_hash(input: &str) -> Result<Vec<u8>, StorageError> {
    match input {
        _ if let Ok(string) = Base16::try_new(input) => {
            base16::decode(&string.into_inner()).context(error::InvalidBase16Snafu)
        }
        _ if let Ok(string) = Base64Standard::try_new(input) => STANDARD_PAD_INDIFFERENT
            .decode(string.into_inner())
            .context(error::InvalidBase64Snafu),
        _ if let Ok(string) = Base64URLSafe::try_new(input) => URL_SAFE_PAD_INDIFFERENT
            .decode(string.into_inner())
            .context(error::InvalidBase64Snafu),
        _ => error::UnknownEncodingSnafu { input }.fail(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Algorithm {
    Sha256, // Sha-2 256-bits
    Sha512, // Sha-2 512-bits
    Blake3, // Blake3
}

impl<'a> TryFrom<&'a str> for Algorithm {
    type Error = StorageError;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        let lower = value.to_lowercase();
        match lower.as_str() {
            "sha256" => Ok(Algorithm::Sha256),
            "sha512" => Ok(Algorithm::Sha512),
            "blake3" => Ok(Algorithm::Blake3),
            algo => error::UnsupportedAlgorithmSnafu { algo }.fail(),
        }
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
            Self::Blake3 => "blake3",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    algorithm: Algorithm,
    hash: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum DigestBuilder {
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Blake(blake3::Hasher),
}

impl Digest {
    /// Creates a digest builder with the default hasing
    /// algorithm (sha256)
    pub fn builder() -> DigestBuilder {
        Self::sha256()
    }

    /// Creates a sha256 digest builder
    fn sha256() -> DigestBuilder {
        DigestBuilder::Sha256(Sha256::new())
    }

    /// Creates a sha512 digest builder
    fn sha512() -> DigestBuilder {
        DigestBuilder::Sha512(Sha512::new())
    }

    /// Creates a blake3 digest builder
    pub fn blake3() -> DigestBuilder {
        DigestBuilder::Blake(blake3::Hasher::new())
    }

    /// Creates a new digest with the provided algorithm
    pub fn with_algorithm(algo: &Algorithm) -> DigestBuilder {
        match algo {
            Algorithm::Blake3 => Self::blake3(),
            Algorithm::Sha256 => Self::sha256(),
            Algorithm::Sha512 => Self::sha512(),
        }
    }

    /// Algorithm this digest uses
    pub fn algorithm(&self) -> &Algorithm {
        &self.algorithm
    }

    /// Hash inside the digest
    pub fn hash(&self) -> &[u8] {
        self.hash.as_slice()
    }

    // Return as a path
    pub fn as_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "{}/{}",
            self.algorithm,
            base16::encode_lower(self.hash.as_slice())
        ))
    }
}

impl DigestBuilder {
    pub fn update(&mut self, input: impl AsRef<[u8]>) -> &mut Self {
        match self {
            Self::Sha256(digest) => digest.update(input),
            Self::Sha512(digest) => digest.update(input),
            Self::Blake(digest) => {
                digest.update(input.as_ref());
            }
        }
        self
    }

    pub fn build(&self) -> Digest {
        match self {
            Self::Sha256(digest) => Digest {
                algorithm: Algorithm::Sha256,
                hash: digest.clone().finalize().to_vec(),
            },
            Self::Sha512(digest) => Digest {
                algorithm: Algorithm::Sha512,
                hash: digest.clone().finalize().to_vec(),
            },
            Self::Blake(digest) => Digest {
                algorithm: Algorithm::Blake3,
                hash: digest.clone().finalize().as_bytes().to_vec(),
            },
        }
    }
}

impl TryFrom<ocilot::digest::Digest> for Digest {
    type Error = StorageError;

    fn try_from(value: ocilot::digest::Digest) -> Result<Self, Self::Error> {
        let algorithm = match value.algorithm() {
            "sha256" => Ok(Algorithm::Sha256),
            "sha512" => Ok(Algorithm::Sha256),
            "blake3" => Ok(Algorithm::Blake3),
            algo => error::UnsupportedAlgorithmSnafu { algo }.fail(),
        }?;
        Ok(Self {
            algorithm,
            hash: base16::decode(value.hex()).context(error::InvalidBase16Snafu)?,
        })
    }
}

impl<'a> TryInto<ocilot::digest::Digest> for &'a Digest {
    type Error = StorageError;

    fn try_into(self) -> Result<ocilot::digest::Digest, Self::Error> {
        let string = self.to_string();
        ocilot::digest::Digest::parse(&string).context(error::OCITranslateSnafu)
    }
}

impl Into<PathBuf> for Digest {
    fn into(self) -> PathBuf {
        self.as_path()
    }
}

impl<'a> TryFrom<&'a str> for Digest {
    type Error = StorageError;

    fn try_from(input: &'a str) -> Result<Self, Self::Error> {
        let (algo, h) = input
            .split_once(":")
            .context(error::DigestNoAlgorithmSnafu)?;
        let algorithm = Algorithm::try_from(algo)?;
        let hash = deserialize_hash(h)?;
        Ok(Self { algorithm, hash })
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input: String = String::deserialize(deserializer)?;
        Self::try_from(input.as_str()).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{}:{}",
            self.algorithm,
            base16::encode_lower(self.hash.as_slice())
        ))
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let string = self.to_string();
        serializer.serialize_str(&string)
    }
}
