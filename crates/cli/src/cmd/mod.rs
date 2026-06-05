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
    target: &str,
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
    let console_cfg = edo::context::ConsoleConfig {
        mode: args.console_mode,
        event_log: args.resolve_event_log(),
    };
    let ctx = Context::init(
        args.storage.clone(),
        args.config.clone(),
        variables.clone(),
        verbosity,
        console_cfg,
    )
    .await?;
    // Provenance header: emit before any project loading so the JSONL
    // log and the canvas header both record which `edo` produced this
    // session, what the user asked for, and when. Sequenced ahead of
    // `ProjectLoaded` (emitted by `Project::build`) and `BuildStarted`
    // (emitted by the scheduler).
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ctx.emit(edo::console::ConsoleEvent::SessionStarted {
        edo_version: env!("CARGO_PKG_VERSION").to_string(),
        target: target.to_string(),
        args: variables.into_iter().collect(),
        started_at_unix,
    });
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
    // Now load the current project
    ctx.load_project(locked).await?;
    Ok(ctx)
}
