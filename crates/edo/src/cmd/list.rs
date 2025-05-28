use crate::Args;
use crate::Result;
use clap::Parser;
use std::collections::HashMap;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "List all transforms", long_about = None)]
pub struct List {}

impl List {
    pub async fn run(&self, args: Args) -> Result<()> {
        let ctx = super::create_context(&args, HashMap::default(), true).await?;
        ctx.print_transforms();
        Ok(())
    }
}
