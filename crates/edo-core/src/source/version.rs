use std::fmt;

use resolvo::utils::VersionSet;
use semver::{Version, VersionReq};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EdoVersion {
    vendor: String,
    version: Version,
}

impl EdoVersion {
    pub fn new(vendor: &str, version: &Version) -> Self {
        Self {
            vendor: vendor.to_string(),
            version: version.clone(),
        }
    }

    pub fn vendor(&self) -> String {
        self.vendor.clone()
    }

    pub fn version(&self) -> Version {
        self.version.clone()
    }

    pub fn matches(&self, require: &VersionReq) -> bool {
        require.matches(&self.version)
    }
}

impl fmt::Display for EdoVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}-{}", self.vendor, self.version))
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EdoVersionSet(Vec<EdoVersion>);

unsafe impl Send for EdoVersionSet {}
unsafe impl Sync for EdoVersionSet {}

impl EdoVersionSet {
    pub fn new(input: &[EdoVersion]) -> Self {
        Self(input.to_vec())
    }

    pub fn get(&self) -> &[EdoVersion] {
        self.0.as_slice()
    }
}

impl fmt::Display for EdoVersionSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{}",
            self.0
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

impl VersionSet for EdoVersionSet {
    type V = EdoVersion;
}
