use super::error;
use async_trait::async_trait;
use semver::VersionReq;
use snafu::{OptionExt, ensure};

use crate::context::{Addr, Context, FromNode, Node};

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct Dependency {
    pub addr: Addr,
    pub kind: String,
    pub name: String,
    pub version: VersionReq,
    pub vendor: Option<String>,
}

#[async_trait]
impl FromNode for Dependency {
    type Error = super::error::SourceError;

    async fn from_node(
        addr: &Addr,
        node: &Node,
        _ctx: &Context,
    ) -> std::result::Result<Self, Self::Error> {
        let id = node.get_id().context(error::UndefinedSnafu)?;
        ensure!(id == "wants", error::UndefinedSnafu {});
        node.validate_keys(&["at"])?;
        let kind = node.get_kind().context(error::UndefinedSnafu)?;
        let name = node.get_name().context(error::UndefinedSnafu)?;
        let version = node
            .get("at")
            .context(error::NoRequireSnafu)?
            .as_require()
            .context(error::FieldSnafu {
                field: "at",
                type_: "version requirement",
            })?;
        let vendor = if let Some(value) = node.get("vendor") {
            Some(
                value
                    .as_string()
                    .context(error::FieldSnafu {
                        field: "vendor",
                        type_: "string",
                    })?
                    .clone(),
            )
        } else {
            None
        };
        Ok(Self {
            addr: addr.clone(),
            kind: kind.clone(),
            name: name.clone(),
            version: version.clone(),
            vendor,
        })
    }
}
