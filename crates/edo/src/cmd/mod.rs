mod checkout;
mod list;
mod prune;
mod run;
mod update;
mod util;

use std::collections::HashMap;

pub use checkout::*;
use edo_core::context::{Context, LogVerbosity};
pub use list::*;
pub use prune::*;
pub use run::*;
pub use update::*;

use crate::Args;
use crate::Result;

pub async fn create_context(
    args: &Args,
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
    Context::init(
        args.storage.clone(),
        args.config.clone(),
        locked,
        variables,
        verbosity,
    )
    .await
    .map_err(|e| e.into())
}
