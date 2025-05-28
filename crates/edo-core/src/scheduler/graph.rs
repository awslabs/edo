use super::execute::execute;
use super::node::Node;
use super::{Result, error};
use crate::context::{Addr, Context, Handle, Log};
use crate::storage::Artifact;
use crate::transform::Transform;
use async_recursion::async_recursion;
use bimap::BiHashMap;
use daggy::petgraph::Direction;
use daggy::petgraph::visit::IntoNeighborsDirected;
use daggy::petgraph::visit::IntoNodeReferences;
use daggy::{Dag, NodeIndex, Walker};
use dashmap::DashMap;
use futures::future::try_join_all;
use snafu::OptionExt;
use snafu::ResultExt;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::ops::Index;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, atomic::Ordering};
use tempfile::TempDir;
use tokio::task::{JoinError, JoinHandle};
use tracing::Instrument;

#[derive(Clone)]
pub struct Graph {
    graph: Dag<Arc<Node>, String>,
    batch_size: u64,
    index: BiHashMap<Addr, NodeIndex>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new(8)
    }
}

unsafe impl Send for Graph {}
unsafe impl Sync for Graph {}

impl Graph {
    pub fn new(batch_size: u64) -> Self {
        trace!(
            component = "execution",
            "creating new execution graph with batch_size={batch_size}"
        );
        Self {
            graph: Dag::new(),
            batch_size,
            index: BiHashMap::new(),
        }
    }

    #[async_recursion]
    pub async fn add(&mut self, ctx: &Context, addr: &Addr) -> Result<NodeIndex> {
        // If we already have this node don't register this
        if let Some(index) = self.index.get_by_left(addr) {
            return Ok(*index);
        }
        trace!(component = "execution", "adding execution node for {addr}");
        let transform = ctx
            .get_transform(addr)
            .context(error::ProjectTransformSnafu { addr: addr.clone() })?;
        let node_index = self.graph.add_node(Arc::new(Node::new(addr)));
        self.index.insert(addr.clone(), node_index);

        // Create edges for all the dependencies
        for dep in transform.depends().await? {
            let child = self.add(ctx, &dep).await?;
            trace!(component = "execution", "adding edge for {dep} -> {addr}");
            self.graph
                .add_edge(child, node_index, format!("{dep}->{addr}"))
                .context(error::GraphSnafu)?;
        }
        Ok(node_index)
    }

    pub async fn fetch(&self, ctx: &Context) -> Result<()> {
        // Now we can parallel iterate to do the fetch
        let mut tasks = Vec::new();
        let ctx = ctx.get_handle();
        for node in self.graph.node_references() {
            let node = node.1.clone();
            let transform = ctx.get(&node.addr).context(error::ProjectTransformSnafu {
                addr: node.addr.clone(),
            })?;
            let id = transform.get_unique_id(&ctx).await?;
            if ctx.storage().find_build(&id, true).await?.is_some() {
                info!("skipped fetch for built entry {}", node.addr);
                continue;
            }
            let ctx = ctx.clone();
            let node = node.clone();
            // We create a logfile here with just the id name so that below when we
            // run we actually will end up reusing this log :D
            tasks.push(tokio::spawn(async move {
                let logf = ctx.log().create(format!("{id}").as_str()).await?;
                logf.set_subject("fetch");
                transform.prepare(&logf, &ctx).await?;
                info!("pulled sources and artifacts for {}", node.addr);
                drop(logf);
                Ok::<(), error::SchedulerError>(())
            }));
        }
        wait(tasks).await?;
        Ok(())
    }

    pub async fn run(&self, path: &Path, ctx: &Context, addr: &Addr) -> Result<()> {
        // Check if this address is already built, if so then do nothing
        let ctx = ctx.get_handle();
        let transform = ctx
            .get(addr)
            .context(error::ProjectTransformSnafu { addr: addr.clone() })?;
        let id = transform.get_unique_id(&ctx).await?;
        if ctx.storage().find_build(&id, false).await?.is_some() {
            info!("{addr} is already built, skipping...");
            return Ok(());
        }
        // Before we start our run we want to simplify the execution graph
        let graph = self.graph.clone();
        let start = self.index.get_by_left(addr).unwrap();

        // We use find_leafs to find the first set of leaf nodes
        let leafs = Self::find_leafs(&graph, start).unwrap_or_default();

        // Now we can execute our task graph. To do so we want to do a maximum optimization of
        // the allowed workers, this means live dispatching new tasks when one finishes if they have parents that
        // can run.

        // Every tokio task spawned must be awaited on so we have a table of JoinHandles
        // here for each node index.
        let handles: Arc<DashMap<NodeIndex<u32>, JoinHandle<Result<Artifact>>>> =
            Arc::new(DashMap::new());

        // We initialize a Dequeue from the first set of leafs, this dequeue will act as our in-progress queue
        let dequeue = Arc::new(tokio::sync::Mutex::new(VecDeque::from_iter(leafs.clone())));

        // We want to one time toggle all of the dequeue nodes into queue state
        let lock = dequeue.lock().await;
        for index in lock.iter() {
            let node = self.graph.index(*index);
            node.set_queued();
        }
        drop(lock);

        // We also track an atomic counter of how many tasks are currently executing
        let inflight = Arc::new(tokio::sync::Mutex::new(AtomicUsize::new(0)));

        // We need a mpsc channel that is buffered to our batch size, this is used to communicate between each individual
        // task coroutine and the controller parent task.
        let (sender, mut receiver) = tokio::sync::mpsc::channel(self.batch_size as usize);

        // Clone handles to relevant objects and spawn the controller parent task
        // this task will handle receiving all results, checking error status and also
        // adding the next set of tasks to the dequeue
        let parent_handle = handles.clone();
        let parent_inflight = inflight.clone();
        let parent_deq = dequeue.clone();
        let parent = tokio::spawn(async move {
            // Iterate on receiving nodes that have completed
            debug!(thread = "queue", "starting queue receiver");
            let mut failure_occured = false;
            while let Some(index) = receiver.recv().await {
                debug!(thread = "queue", "received notice that {:?} is done", index);
                // Now before continuing we need to check if the node is in success or not
                let node: &Arc<Node> = graph.index(index);
                debug!(thread = "queue", "determined node to be {}", node.addr);
                // Check if this node has a join handle, the only case it will not
                // is if the task was pre-built
                if let Some(handle) = parent_handle.remove(&index) {
                    debug!(thread = "queue", "waiting on the handle");
                    // Ensure to await here, if you do not runtime can cancel tasks leading to issues
                    match handle.1.await.context(error::JoinSnafu)? {
                        Ok(_) => node.set_success(),
                        Err(e) => {
                            error!("{} failed: {e}", node.addr);
                            node.set_failed();
                            failure_occured = true;
                            continue;
                        }
                    }
                } else {
                    // In the prebuilt case always flag success
                    node.set_success();
                }

                if failure_occured {
                    // If a failure has occured do not keep walking the dag
                    continue;
                }

                // Now evaluate the graph to find any parents that this finished node have that are now
                // ready to execute and add them to the queue.
                let mut children = graph.children(index);
                while let Some(child) = children.walk_next(&graph) {
                    // Now check if we have a parent that still hasn't run
                    let mut parents = graph.parents(child.1);
                    let node = graph.index(child.1);
                    let mut skip = false;
                    while let Some(parent) = parents.walk_next(&graph) {
                        let pnode = graph.index(parent.1);
                        if pnode.is_queued() || pnode.is_pending() {
                            debug!(
                                thread = "queue",
                                "determined that parent {} is still not done so {} won't be queue'd",
                                pnode.addr,
                                node.addr
                            );
                            skip = true;
                            break;
                        }
                    }
                    if skip {
                        continue;
                    }
                    // Now make sure that this child has not executed already
                    if node.is_pending() {
                        debug!(thread = "queue", "determined we can execute: {}", node.addr);
                        // Mark it so we know it is in the queue to prevent duplicates
                        node.set_queued();
                        parent_deq.lock().await.push_back(child.1);
                    } else {
                        debug!(thread = "queue", "{} is not pending", node.addr);
                    }
                }
                // We decrease the inflight here to prevent a race condition and ensure all nodes are visited
                parent_inflight.lock().await.fetch_sub(1, Ordering::SeqCst);
            }
            Ok::<(), error::SchedulerError>(())
        });

        // Now we are ready to have our dispatch loop which is handled by the current scope
        let loop_deq = dequeue.clone();
        let loop_inflight = inflight.clone();
        // Iterate as long as the queue is not empty and there are tasks in flight, the inflight count will only go fully 0 once
        // we are done
        while !loop_deq.lock().await.is_empty()
            || loop_inflight.lock().await.load(Ordering::SeqCst) > 0
        {
            let current = loop_inflight.lock().await.load(Ordering::SeqCst);
            // We want to ensure we only ever have at max batch-size tasks in operation
            let send_amount = std::cmp::min(
                self.batch_size - current as u64,
                loop_deq.lock().await.len() as u64,
            );
            // For how many slots open dispatch the next tasks
            for _ in 0..send_amount {
                if let Some(index) = dequeue.lock().await.pop_front() {
                    let node = self.graph.index(index).clone();
                    let addr = node.addr.clone();
                    let path = path.to_path_buf();
                    let handles = handles.clone();
                    let transform = ctx
                        .get(&addr)
                        .context(error::ProjectTransformSnafu { addr: addr.clone() })?;
                    let id = transform.get_unique_id(&ctx).await?;
                    let sender = sender.clone();
                    inflight.lock().await.fetch_add(1, Ordering::SeqCst);
                    if ctx.storage().find_build(&id, false).await?.is_some() {
                        info!("{addr} is already built, skipping...");
                        sender.send(index).await.context(error::SignalSnafu)?;
                        break;
                    }
                    let ctx = ctx.clone();
                    let node = node.clone();
                    handles.insert(
                        index,
                        tokio::spawn(async move {
                            trace!(component = "execution", "performing transform {addr}");
                            let logf = ctx.log().create(format!("{id}").as_str()).await?;
                            node.set_running();
                            let result = Self::transform(&logf, &path, &ctx, &addr, &transform)
                                .instrument(info_span!("transforming", addr = addr.to_string()))
                                .await;
                            drop(logf);
                            sender.send(index).await.context(error::SignalSnafu)?;
                            result
                        }),
                    );
                } else {
                    // If we've reached the end of a wave exit out
                    break;
                }
            }
        }
        // We need to make sure we drop the sender here to avoid hanging on the final task
        drop(sender);
        // We now need to await on the parent to exit
        parent.await.context(error::JoinSnafu)??;

        Ok(())
    }

    fn find_leafs(graph: &Dag<Arc<Node>, String>, index: &NodeIndex) -> Option<HashSet<NodeIndex>> {
        let mut leafs = HashSet::new();
        let mut count = 0;
        for node in graph.neighbors_directed(*index, Direction::Incoming) {
            if let Some(children) = Self::find_leafs(graph, &node) {
                for entry in children {
                    leafs.insert(entry);
                }
            }
            count += 1;
        }
        if count == 0 {
            // if we didn't have any leaf nodes discovered by this node's children than this is a leaf node
            leafs.insert(*index);
        }
        if leafs.is_empty() { None } else { Some(leafs) }
    }

    async fn transform(
        log: &Log,
        workspace: &Path,
        ctx: &Handle,
        addr: &Addr,
        transform: &Transform,
    ) -> Result<Artifact> {
        let temp = TempDir::new_in(workspace).context(error::TemporaryDirectorySnafu)?;
        log.set_subject("create-environment");
        let env_addr = transform.environment().await?;
        let environment = ctx
            .create_environment(log, &env_addr, temp.path())
            .instrument(info_span!("creating environment", addr = addr.to_string(),))
            .await?;
        // Setup the environment
        log.set_subject("setup-environment");
        environment
            .setup(log, ctx.storage())
            .instrument(info_span!(
                "setting up environment",
                addr = addr.to_string()
            ))
            .await?;
        info!("created environment");

        // Bring the environment up
        log.set_subject("spinup environment");
        environment.up(log).await?;

        // Stage transform
        log.set_subject("staging");
        transform
            .stage(log, ctx, &environment)
            .instrument(info_span!(
                "staging into environment",
                addr = addr.to_string()
            ))
            .await?;
        info!("staged dependencies and sources");

        // Perform the transform
        log.set_subject("execution");
        let artifact = execute(log, ctx, transform, &environment)
            .instrument(info_span!("transforming", addr = addr.to_string()))
            .await;

        // Shutdown the environment
        log.set_subject("spindown environment");
        environment.down(log).await?;

        // Clean the environment
        log.set_subject("clean environment");
        environment
            .clean(log)
            .instrument(info_span!("cleaning up", addr = addr.to_string(),))
            .await?;

        match artifact {
            Ok(artifact) => {
                info!("transformation complete");
                Ok(artifact)
            }
            Err(e) => {
                error!("transformation failed: {e}");
                Err(e)
            }
        }
    }
}

async fn wait<I, R>(handles: I) -> Result<Vec<R>>
where
    R: Clone,
    I: IntoIterator,
    I::Item: Future<Output = std::result::Result<Result<R>, JoinError>>,
{
    let result = try_join_all(handles).await;
    let mut success = Vec::new();
    let mut failures = Vec::new();
    for entry in result.context(error::JoinSnafu)? {
        match entry {
            Ok(result) => success.push(result),
            Err(e) => failures.push(e),
        }
    }
    if !failures.is_empty() {
        error::ChildSnafu { children: failures }.fail()
    } else {
        Ok(success)
    }
}
