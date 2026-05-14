pub mod cargo_vendor;
pub mod compose;
pub mod go_vendor;
pub mod import;
pub mod script;

use edo::context::{Addr, Context, ContextError, Node};
use edo::source::Source;
use indexmap::IndexMap;

pub use cargo_vendor::CargoVendorTransform;
pub use compose::ComposeTransform;
pub use go_vendor::GoVendorTransform;
pub use import::ImportTransform;
pub use script::ScriptTransform;

/// Parses the `source` list from a transform node and registers each source with the context.
pub async fn parse_sources<E, F>(
    addr: &Addr,
    node: &Node,
    ctx: &Context,
    field_error: F,
) -> Result<IndexMap<String, Source>, E>
where
    E: snafu::Error + From<ContextError>,
    F: Fn(&str, &str) -> E,
{
    parse_sources_with_name("source", addr, node, ctx, field_error).await
}

pub async fn parse_sources_with_name<E, F>(
    field: &str,
    addr: &Addr,
    node: &Node,
    ctx: &Context,
    field_error: F,
) -> Result<IndexMap<String, Source>, E>
where
    E: snafu::Error + From<ContextError>,
    F: Fn(&str, &str) -> E,
{
    let mut sources = IndexMap::new();
    let standin = Node::new_list(vec![]);
    let list = node
        .get(field)
        .unwrap_or(standin)
        .as_list()
        .ok_or(field_error("source", "source definition"))?;
    for node in list.iter() {
        let source = ctx.add_source(addr, node).await?;
        let name = node.get_name().unwrap();
        sources.insert(name, source);
    }
    Ok(sources)
}

/// Parses a dependency list from the given node key into a vector of addresses.
pub async fn parse_depends<E, F>(node: &Node, key: &str, field_error: F) -> Result<Vec<Addr>, E>
where
    E: snafu::Error + From<ContextError>,
    F: Fn(&str, &str) -> E,
{
    let mut depends = Vec::new();
    for entry in node
        .get(key)
        .unwrap_or(Node::new_list(Vec::new()))
        .as_list()
        .ok_or(field_error(key, "list of strings"))?
    {
        let value = entry.as_string().ok_or(field_error(key, "string"))?;
        let addr = Addr::parse(value.as_str())?;
        depends.push(addr);
    }
    Ok(depends)
}
