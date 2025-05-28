use std::collections::HashMap;

use crate::Result;
use clap::Parser;
use edo_core::context::Addr;

use crate::Args;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Run a transform", long_about = None)]
pub struct Run {
    addr: String,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl Run {
    pub async fn run(&self, args: Args) -> Result<()> {
        let ctx = super::create_context(
            &args,
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
        )
        .await?;
        let addr = Addr::parse(self.addr.as_str())?;
        ctx.run(&addr).await?;
        Ok(())
    }
}
