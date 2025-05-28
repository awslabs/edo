use std::collections::HashMap;

use crate::Result;
use clap::Parser;

use crate::Args;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Prune latent information", long_about = None)]
pub struct Prune {
    #[arg(short, long)]
    all: bool,
    // Prune the logs as well
    #[arg(short, long)]
    logs: bool,
}

impl Prune {
    pub async fn run(&self, args: Args) -> Result<()> {
        let ctx = super::create_context(&args, HashMap::default(), true).await?;
        // Prune the local cache
        if self.all {
            ctx.storage().prune_local_all().await?;
        } else {
            ctx.prune().await?;
        }
        if self.logs || self.all {
            ctx.log().clear().await?;
        }
        Ok(())
    }
}
