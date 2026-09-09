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
    let ctx = Context::init(
        args.storage.clone(),
        args.config.clone(),
        variables.clone(),
        verbosity,
        args.event_log_setting(),
    )
    .await?;
    // Provenance header: emit before any project loading so the JSONL
    // log and terminal both record which `edo` produced this session,
    // what the user asked for, and when.
    //
    // `target` is `Some(addr)` only for commands that operate on a
    // specific transform (`run`, `checkout`). Session commands like
    // `update`, `list`, and `prune` have no build target; the no-addr
    // variant of `header!` keeps the `target:` line out of the log in
    // that case.
    let version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    let args_vec: Vec<(String, String)> = variables.into_iter().collect();
    if let Some(t) = target {
        edo::header!("edo-ref" @ &version => t, args_vec);
    } else {
        edo::header!("edo-ref" @ &version, args_vec);
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
