pub mod compose;
pub mod import;
pub mod script;

use crate::context::{Addr, Context, ContextError, Node};
use crate::source::Source;
use indexmap::IndexMap;

pub use compose::ComposeTransform;
pub use import::ImportTransform;
pub use script::ScriptTransform;

async fn parse_sources<E, F>(
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
        .get("source")
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

async fn parse_depends<E, F>(node: &Node, key: &str, field_error: F) -> Result<Vec<Addr>, E>
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
