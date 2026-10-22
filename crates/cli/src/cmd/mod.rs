mod checkout;
mod list;
mod prune;
mod run;
mod update;
mod util;

use std::collections::{BTreeMap, HashMap};

pub use checkout::*;
use edo::context::Element;
use edo::context::{Addr, Context, LogVerbosity};
use edo_core::register_core;
pub use list::*;
pub use prune::*;
pub use run::*;
pub use update::*;

use crate::Args;
use crate::Result;

pub async fn create_context(
    args: &Args,
    target: Option<&str>,
    variables: HashMap<String, String>,
    locked: bool,
) -> Result<Context> {
    let verbosity = if args.trace {
        LogVerbosity::Trace
    } else if args.debug {
        LogVerbosity::Debug
    } else {
        LogVerbosity::Info
    };
    // Only argument names are recorded: values may carry secrets and the
    // header is persisted to the event log.
    let mut arg_keys: Vec<String> = variables.keys().cloned().collect();
    arg_keys.sort_unstable();
    let ctx = Context::init_with_cancellation(
        args.storage.clone(),
        args.config.clone(),
        variables,
        verbosity,
        args.event_log.clone(),
        args.cancellation.clone(),
    )
    .await?;
    // Emitted before project loading so every session records which `edo` produced it.
    let tool = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    if let Some(target) = target {
        edo::header!(
            tool = tool,
            version = version,
            target = target,
            arg_keys = arg_keys
        );
    } else {
        edo::header!(tool = tool, version = version, arg_keys = arg_keys);
    }
    // Register all core component handlers
    register_core(&ctx);
    // Register a local farm in the project directory
    let local_farm_addr = Addr::parse("//default").unwrap();
    ctx.add_farm(
        &Element::builder()
            .kind("local")
            .addr(local_farm_addr)
            .config(BTreeMap::default())
            .build(),
    )
    .await?;
    // Load the current project.
    ctx.load_project(locked).await?;
    Ok(ctx)
}
