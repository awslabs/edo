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
    // project loading and the scheduler's start-build event.
    let version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    let target_addr = Addr::parse(target).ok();
    let args_vec: Vec<(String, String)> = variables.into_iter().collect();
    if let Some(c) = edo::ui::Console::global() {
        c.emit_header("edo-ref", &version, target_addr, args_vec)
            .await;
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
    // Now load the current project
    ctx.load_project(locked).await?;
    Ok(ctx)
}
