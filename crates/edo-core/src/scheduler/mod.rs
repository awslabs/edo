use graph::Graph;
use tokio::fs::create_dir_all;

use crate::context::{Addr, Config};
use snafu::ResultExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use super::context::Context;

pub mod error;
mod execute;
mod graph;
mod node;

type Result<T> = std::result::Result<T, error::SchedulerError>;

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    pub async fn new<P: AsRef<Path>>(path: P, config: &Config) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            create_dir_all(path).await.context(error::IoSnafu)?;
        }
        let workers = if let Some(node) = config.get("scheduler") {
            node.get("workers").and_then(|x| x.as_int())
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(Inner {
                workers: if let Some(workers) = workers {
                    workers as u64
                } else {
                    8
                },
                path: path.to_path_buf(),
            }),
        })
    }
}

impl Scheduler {
    pub async fn run(&self, ctx: &Context, addr: &Addr) -> Result<()> {
        self.inner.run(ctx, addr).await
    }
}

struct Inner {
    path: PathBuf,
    workers: u64,
}

impl Inner {
    pub async fn run(&self, ctx: &Context, addr: &Addr) -> Result<()> {
        let mut graph = Graph::new(self.workers);
        graph.add(ctx, addr).await?;
        graph.fetch(ctx).await?;
        let graph_ref = Arc::new(graph);
        graph_ref.run(&self.path, ctx, addr).await?;
        Ok(())
    }
}
