use std::collections::HashMap;

use crate::Args;
use crate::Result;
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Update edo lock to latest state", long_about = None)]
pub struct Update {}

impl Update {
    pub async fn run(&self, args: Args) -> Result<()> {
        let _ = super::create_context(&args, HashMap::default(), false).await?;
        Ok(())
    }
}
