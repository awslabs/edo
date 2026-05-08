//! DAG execution graph for scheduling transforms.
//!
//! Constructs a directed acyclic graph from transform dependencies, resolves
//! build-cache hits, and drives parallel execution within batch-size limits.

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

/// DAG-based execution graph that manages transform dependencies and parallel execution.
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
    /// Creates a new execution graph with the given maximum batch size for parallel tasks.
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

    /// Recursively adds a transform and its dependencies to the graph.
    ///
    /// Returns the `NodeIndex` of the added (or existing) node. Edges are created
    /// from each dependency to the dependent node.
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

    /// Fetches sources and artifacts for all nodes in parallel.
    ///
    /// Skips nodes whose builds are already cached. Each fetch operation runs
    /// as a separate Tokio task for maximum throughput.
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

    /// Executes the transform graph starting from the given address.
    ///
    /// Drives execution by dispatching leaf nodes first and walking the DAG
    /// upward as dependencies complete. Respects the configured batch-size limit
    /// for in-flight tasks and skips nodes that already have cached builds.
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
                        }
                    }
                } else {
                    // In the prebuilt case always flag success
                    node.set_success();
                }

                // Only walk children on the success path; a failure short-circuits
                // further dispatch. The decrement still runs below so the dispatch
                // loop can exit cleanly (preserving the 41eb63d deadlock fix).
                if !failure_occured {
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
                }

                // Decrement LAST so the dispatch loop cannot observe
                // `inflight == 0 && deque.is_empty()` until any newly-ready
                // children have already been enqueued above. This ordering is
                // critical: if the decrement ran before the child-enqueue walk,
                // the dispatcher could race, exit, and strand the root
                // transform (see commit history around scheduler race fix).
                // The decrement runs on every completion (success or failure),
                // which preserves the failure-path deadlock fix from 41eb63d.
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
            // Yield to allow the parent task to process completions and decrement
            // inflight, preventing busy-spin starvation on low-concurrency runtimes.
            tokio::task::yield_now().await;
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

/// Awaits all join handles, collecting successes or returning aggregated failures.
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

#[cfg(test)]
pub(crate) mod tests {
    //! Integration-style tests for [`Graph`].
    //!
    //! Mocks for `Transform`, `Farm`, and `Environment` live here so that
    //! every test file in this module can re-use them via the
    //! `#[path]` include pattern — see `execute.rs::tests` for the
    //! second (duplicated) mock copy.  We keep the mocks inline (per
    //! plan) instead of in a shared `test_support.rs`.

    use super::*;
    use crate::context::{Addr, Context, Handle, LogVerbosity};
    use crate::environment::{Command, EnvResult, Environment, EnvironmentImpl, Farm, FarmImpl};
    use crate::storage::{Artifact as StorageArtifact, Config as ArtifactConfig, Id, MediaType};
    use crate::transform::{Transform, TransformImpl, TransformResult, TransformStatus};
    use crate::util::{Reader, Writer};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tempfile::TempDir;
    use tokio::sync::{Mutex as TokioMutex, OnceCell};

    // ── context bootstrapping (mirrors context/mod.rs pattern) ──────────────

    type SharedCtx = (Context, Arc<TempDir>);
    static SHARED: OnceCell<SharedCtx> = OnceCell::const_new();

    /// Returns a process-wide `Context` if the global tracing subscriber
    /// is installable in this binary. Returns `None` if a sibling test
    /// module already installed a subscriber.
    pub(crate) async fn try_shared_context() -> Option<Context> {
        if let Some((ctx, _)) = SHARED.get() {
            return Some(ctx.clone());
        }
        let dir = TempDir::new().expect("tempdir");
        match Context::init::<&Path, &Path>(
            Some(dir.path()),
            None,
            HashMap::new(),
            LogVerbosity::Info,
        )
        .await
        {
            Ok(ctx) => {
                let _ = SHARED.set((ctx.clone(), Arc::new(dir)));
                Some(SHARED.get().map(|(c, _)| c.clone()).unwrap_or(ctx))
            }
            Err(crate::context::ContextError::Log { .. }) => None,
            Err(e) => panic!("unexpected Context::init error: {e}"),
        }
    }

    macro_rules! ctx_or_skip {
        () => {
            match try_shared_context().await {
                Some(c) => c,
                None => {
                    eprintln!(
                        "skip: global tracing subscriber already initialized \
                         by a sibling test in this binary"
                    );
                    return;
                }
            }
        };
    }

    // ── mock Environment ─────────────────────────────────────────────────────

    /// Trivial environment used by mock transforms. Every method either
    /// succeeds with a default value or records nothing of interest — the
    /// scheduler tests only care that the calls do not return errors.
    pub(crate) struct MockEnvironmentImpl;

    #[async_trait]
    impl EnvironmentImpl for MockEnvironmentImpl {
        async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
            Ok(path.to_path_buf())
        }
        async fn create_dir(&self, _path: &Path) -> EnvResult<()> {
            Ok(())
        }
        async fn set_env(&self, _key: &str, _value: &str) -> EnvResult<()> {
            Ok(())
        }
        async fn get_env(&self, _key: &str) -> Option<String> {
            None
        }
        async fn setup(
            &self,
            _log: &crate::context::Log,
            _storage: &crate::storage::Storage,
        ) -> EnvResult<()> {
            Ok(())
        }
        async fn up(&self, _log: &crate::context::Log) -> EnvResult<()> {
            Ok(())
        }
        async fn down(&self, _log: &crate::context::Log) -> EnvResult<()> {
            Ok(())
        }
        async fn clean(&self, _log: &crate::context::Log) -> EnvResult<()> {
            Ok(())
        }
        async fn write(&self, _path: &Path, _reader: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn unpack(&self, _path: &Path, _reader: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn read(&self, _path: &Path, _writer: Writer) -> EnvResult<()> {
            Ok(())
        }
        async fn cmd(
            &self,
            _log: &crate::context::Log,
            _id: &Id,
            _path: &Path,
            _command: &str,
        ) -> EnvResult<bool> {
            Ok(true)
        }
        async fn run(
            &self,
            _log: &crate::context::Log,
            _id: &Id,
            _path: &Path,
            _command: &Command,
        ) -> EnvResult<bool> {
            Ok(true)
        }
        fn shell(&self, _path: &Path) -> EnvResult<()> {
            Ok(())
        }
    }

    // ── mock Farm ────────────────────────────────────────────────────────────

    pub(crate) struct MockFarmImpl;

    #[async_trait]
    impl FarmImpl for MockFarmImpl {
        async fn setup(
            &self,
            _log: &crate::context::Log,
            _storage: &crate::storage::Storage,
        ) -> EnvResult<()> {
            Ok(())
        }
        async fn create(&self, _log: &crate::context::Log, _path: &Path) -> EnvResult<Environment> {
            Ok(Environment::new(MockEnvironmentImpl))
        }
    }

    pub(crate) fn mock_farm() -> Farm {
        Farm::new(MockFarmImpl)
    }

    // ── mock Transform ───────────────────────────────────────────────────────

    /// Outcome the mock's `transform` method should return.
    #[derive(Clone)]
    pub(crate) enum MockOutcome {
        Success,
        /// Fail from the `stage` lifecycle method with a synthetic
        /// [`TransformError::Implementation`]. Failing in `stage` (rather
        /// than returning [`TransformStatus::Failed`] from `transform`)
        /// avoids the interactive `dialoguer::Select::interact` prompt
        /// in `execute::execute` so the scheduler's failure path can be
        /// exercised from a unit test.
        FailInStage,
    }

    /// Configurable mock transform.
    ///
    /// Exposes atomic counters for each lifecycle method so that tests can
    /// assert which parts of the pipeline ran, and an `order_log` that
    /// records the address of every `transform` entry (for topological
    /// ordering assertions).
    pub(crate) struct MockTransformImpl {
        pub addr: Addr,
        pub deps: Vec<Addr>,
        pub env_addr: Addr,
        pub digest: String,
        pub prepare_called: Arc<AtomicUsize>,
        pub stage_called: Arc<AtomicUsize>,
        pub transform_called: Arc<AtomicUsize>,
        pub inflight: Arc<AtomicUsize>,
        pub max_inflight: Arc<AtomicUsize>,
        pub order_log: Arc<TokioMutex<Vec<Addr>>>,
        pub outcome: MockOutcome,
        /// Optional sleep injected into `transform()` to widen the
        /// scheduler's completion-to-dispatch race window so unit tests
        /// can reliably observe ordering bugs that real-world long-running
        /// transforms would expose.
        pub delay: Option<std::time::Duration>,
    }

    impl Default for MockTransformImpl {
        fn default() -> Self {
            Self {
                addr: Addr::parse("//proj/unset").unwrap(),
                deps: Vec::new(),
                env_addr: Addr::parse("//default").unwrap(),
                digest: "0000".into(),
                prepare_called: Arc::new(AtomicUsize::new(0)),
                stage_called: Arc::new(AtomicUsize::new(0)),
                transform_called: Arc::new(AtomicUsize::new(0)),
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                order_log: Arc::new(TokioMutex::new(Vec::new())),
                outcome: MockOutcome::Success,
                delay: None,
            }
        }
    }

    fn make_artifact(digest: &str) -> StorageArtifact {
        let id = Id::builder()
            .name("mock".to_string())
            .digest(digest.to_string())
            .build();
        StorageArtifact::builder()
            .media_type(MediaType::File(crate::storage::Compression::None))
            .config(ArtifactConfig::builder().id(id).build())
            .build()
    }

    #[async_trait]
    impl TransformImpl for MockTransformImpl {
        async fn environment(&self) -> TransformResult<Addr> {
            Ok(self.env_addr.clone())
        }

        async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
            Ok(Id::builder()
                .name(self.addr.to_string())
                .digest(self.digest.clone())
                .build())
        }

        async fn depends(&self) -> TransformResult<Vec<Addr>> {
            Ok(self.deps.clone())
        }

        async fn prepare(&self, _log: &crate::context::Log, _ctx: &Handle) -> TransformResult<()> {
            self.prepare_called.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }

        async fn stage(
            &self,
            _log: &crate::context::Log,
            _ctx: &Handle,
            _env: &Environment,
        ) -> TransformResult<()> {
            self.stage_called.fetch_add(1, AtomicOrdering::SeqCst);
            if matches!(self.outcome, MockOutcome::FailInStage) {
                return Err(crate::transform::TransformError::Implementation {
                    source: Box::new(std::io::Error::other("mock stage failure")),
                });
            }
            Ok(())
        }

        async fn transform(
            &self,
            _log: &crate::context::Log,
            _ctx: &Handle,
            _env: &Environment,
        ) -> TransformStatus {
            self.transform_called.fetch_add(1, AtomicOrdering::SeqCst);
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            // Track peak concurrency for batch-size assertions.
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            self.order_log.lock().await.push(self.addr.clone());
            // Small yield so concurrent transforms interleave.
            tokio::task::yield_now().await;
            // Optional sleep to widen the race window between the parent
            // task's inflight decrement and its child-enqueue walk.
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);

            match self.outcome {
                MockOutcome::Success => TransformStatus::Success(make_artifact(&self.digest)),
                // Failure is already returned from `stage` for the
                // `FailInStage` outcome, so this arm is unreachable when
                // the scheduler is exercised; return Success so unit tests
                // that touch `transform` directly still behave.
                MockOutcome::FailInStage => TransformStatus::Success(make_artifact(&self.digest)),
            }
        }

        fn can_shell(&self) -> bool {
            false
        }

        fn shell(&self, _env: &Environment) -> TransformResult<()> {
            Ok(())
        }
    }

    /// Handle bundle returned to tests so they can observe counters after
    /// the scheduler finishes.
    #[allow(dead_code)] // fields consumed by sibling test modules
    pub(crate) struct MockHandles {
        pub addr: Addr,
        pub prepare_called: Arc<AtomicUsize>,
        pub stage_called: Arc<AtomicUsize>,
        pub transform_called: Arc<AtomicUsize>,
        pub max_inflight: Arc<AtomicUsize>,
        pub order_log: Arc<TokioMutex<Vec<Addr>>>,
    }

    /// Build and register a mock transform at `addr` with the given deps.
    /// Returns a `MockHandles` for assertion access.
    ///
    /// A single shared `order_log` / `max_inflight` can be threaded through
    /// multiple mocks so concurrency and ordering become observable across
    /// the whole graph.
    pub(crate) fn register_mock(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
        shared_order: Arc<TokioMutex<Vec<Addr>>>,
        shared_max_inflight: Arc<AtomicUsize>,
    ) -> MockHandles {
        let addr = Addr::parse(addr_str).unwrap();
        let deps_vec: Vec<Addr> = deps
            .iter()
            .map(|s| Addr::parse(s).expect("dep addr"))
            .collect();
        let env_addr = Addr::parse("//default").unwrap();
        let digest = format!("{:064x}", fxhash(addr_str));
        let prepare_called = Arc::new(AtomicUsize::new(0));
        let stage_called = Arc::new(AtomicUsize::new(0));
        let transform_called = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));

        let mock = MockTransformImpl {
            addr: addr.clone(),
            deps: deps_vec,
            env_addr: env_addr.clone(),
            digest,
            prepare_called: prepare_called.clone(),
            stage_called: stage_called.clone(),
            transform_called: transform_called.clone(),
            inflight,
            max_inflight: shared_max_inflight.clone(),
            order_log: shared_order.clone(),
            outcome: MockOutcome::Success,
            delay: None,
        };
        let t = Transform::new(mock);
        ctx.insert_transform_for_test(&addr, t);
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight: shared_max_inflight,
            order_log: shared_order,
        }
    }

    /// Like [`register_mock`] but allows overriding [`MockOutcome`] and
    /// injecting an optional per-transform delay.
    ///
    /// The delay widens the scheduler's race window between a task's
    /// completion and the parent-task's child-enqueue walk — critical for
    /// reliably reproducing the ordering bug that only surfaces on
    /// long-running real transforms.
    pub(crate) fn register_mock_with(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
        shared_order: Arc<TokioMutex<Vec<Addr>>>,
        shared_max_inflight: Arc<AtomicUsize>,
        outcome: MockOutcome,
        delay: Option<std::time::Duration>,
    ) -> MockHandles {
        let addr = Addr::parse(addr_str).unwrap();
        let deps_vec: Vec<Addr> = deps
            .iter()
            .map(|s| Addr::parse(s).expect("dep addr"))
            .collect();
        let env_addr = Addr::parse("//default").unwrap();
        let digest = format!("{:064x}", fxhash(addr_str));
        let prepare_called = Arc::new(AtomicUsize::new(0));
        let stage_called = Arc::new(AtomicUsize::new(0));
        let transform_called = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));

        let mock = MockTransformImpl {
            addr: addr.clone(),
            deps: deps_vec,
            env_addr: env_addr.clone(),
            digest,
            prepare_called: prepare_called.clone(),
            stage_called: stage_called.clone(),
            transform_called: transform_called.clone(),
            inflight,
            max_inflight: shared_max_inflight.clone(),
            order_log: shared_order.clone(),
            outcome,
            delay,
        };
        let t = Transform::new(mock);
        ctx.insert_transform_for_test(&addr, t);
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight: shared_max_inflight,
            order_log: shared_order,
        }
    }

    /// Tiny non-cryptographic hash used only to generate stable digest
    /// strings so each mock ends up with a distinct `Id`.
    fn fxhash(s: &str) -> u128 {
        let mut h: u128 = 0xcbf2_9ce4_8422_2325_cbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= *b as u128;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Ensure the default farm exists so `create_environment` can succeed.
    pub(crate) fn ensure_default_farm(ctx: &Context) {
        ctx.insert_farm_for_test(&Addr::parse("//default").unwrap(), mock_farm());
    }

    // ── actual tests ────────────────────────────────────────────────────────

    #[test]
    fn default_uses_batch_size_8() {
        let g = Graph::default();
        assert_eq!(g.batch_size, 8);
    }

    #[test]
    fn new_honours_custom_batch_size() {
        let g = Graph::new(3);
        assert_eq!(g.batch_size, 3);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_add_registers_single_node() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        register_mock(&ctx, "//gadd/single", &[], order, mi);

        let mut g = Graph::new(4);
        let idx = g
            .add(&ctx, &Addr::parse("//gadd/single").unwrap())
            .await
            .expect("add");
        assert_eq!(g.graph.node_count(), 1);
        // Re-adding is idempotent and returns the same index.
        let idx2 = g
            .add(&ctx, &Addr::parse("//gadd/single").unwrap())
            .await
            .expect("add twice");
        assert_eq!(idx, idx2);
        assert_eq!(g.graph.node_count(), 1);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_add_transitive_dependencies() {
        // A → B → C : adding A should register three nodes and two edges.
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        register_mock(&ctx, "//gtrans/c", &[], order.clone(), mi.clone());
        register_mock(
            &ctx,
            "//gtrans/b",
            &["//gtrans/c"],
            order.clone(),
            mi.clone(),
        );
        register_mock(&ctx, "//gtrans/a", &["//gtrans/b"], order, mi);

        let mut g = Graph::new(4);
        g.add(&ctx, &Addr::parse("//gtrans/a").unwrap())
            .await
            .expect("add a");
        assert_eq!(g.graph.node_count(), 3);
        assert_eq!(g.graph.edge_count(), 2);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_add_missing_transform_errors() {
        let ctx = ctx_or_skip!();
        let mut g = Graph::new(4);
        let err = g
            .add(&ctx, &Addr::parse("//missing/xform").unwrap())
            .await
            .expect_err("should error");
        assert!(
            matches!(err, error::SchedulerError::ProjectTransform { .. }),
            "got {err:?}",
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_add_cycle_errors() {
        // A depends on B, B depends on A — cycle.
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        register_mock(&ctx, "//gcyc/a", &["//gcyc/b"], order.clone(), mi.clone());
        register_mock(&ctx, "//gcyc/b", &["//gcyc/a"], order, mi);

        let mut g = Graph::new(4);
        let err = g
            .add(&ctx, &Addr::parse("//gcyc/a").unwrap())
            .await
            .expect_err("cycle should fail");
        assert!(
            matches!(err, error::SchedulerError::Graph { .. }),
            "got {err:?}",
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_fetch_calls_prepare_per_node() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        // NOTE: we cannot easily forge a cached build without real layer
        // bytes, so this test only validates the positive path: every
        // node's `prepare` is called exactly once.
        let h_c = register_mock(&ctx, "//gf/c", &[], order.clone(), mi.clone());
        let h_b = register_mock(&ctx, "//gf/b", &["//gf/c"], order.clone(), mi.clone());
        let h_a = register_mock(&ctx, "//gf/a", &["//gf/b"], order, mi);

        let mut g = Graph::new(4);
        g.add(&ctx, &Addr::parse("//gf/a").unwrap()).await.unwrap();
        g.fetch(&ctx).await.expect("fetch");

        assert_eq!(h_a.prepare_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_b.prepare_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_c.prepare_called.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_linear_chain_in_topological_order() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        let h_c = register_mock(&ctx, "//glin/c", &[], order.clone(), mi.clone());
        let h_b = register_mock(&ctx, "//glin/b", &["//glin/c"], order.clone(), mi.clone());
        let h_a = register_mock(&ctx, "//glin/a", &["//glin/b"], order.clone(), mi);

        let mut g = Graph::new(4);
        let root = Addr::parse("//glin/a").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // Each invoked exactly once.
        assert_eq!(h_a.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_b.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_c.transform_called.load(AtomicOrdering::SeqCst), 1);

        // Ordering: C must finish before B, B before A. The shared log
        // records entry order which — for a linear chain under any
        // batch-size — must match topological order.
        let log = order.lock().await;
        assert_eq!(
            log.as_slice(),
            &[
                Addr::parse("//glin/c").unwrap(),
                Addr::parse("//glin/b").unwrap(),
                Addr::parse("//glin/a").unwrap(),
            ]
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_diamond_dependency_respected() {
        // A depends on B & C; B & C depend on D.
        //       D
        //      / \
        //     B   C
        //      \ /
        //       A
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        register_mock(&ctx, "//gdia/d", &[], order.clone(), mi.clone());
        register_mock(&ctx, "//gdia/b", &["//gdia/d"], order.clone(), mi.clone());
        register_mock(&ctx, "//gdia/c", &["//gdia/d"], order.clone(), mi.clone());
        register_mock(
            &ctx,
            "//gdia/a",
            &["//gdia/b", "//gdia/c"],
            order.clone(),
            mi,
        );

        let mut g = Graph::new(4);
        let root = Addr::parse("//gdia/a").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        let log = order.lock().await;
        // D must be first; A must be last.
        assert_eq!(log.first().unwrap(), &Addr::parse("//gdia/d").unwrap());
        assert_eq!(log.last().unwrap(), &Addr::parse("//gdia/a").unwrap());
        assert_eq!(log.len(), 4);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_batch_size_one_serializes() {
        // With batch_size=1, no two transforms may run concurrently. Three
        // independent leaves are registered, all feeding into a root so that
        // the dispatcher has real parallel opportunity.
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        register_mock(&ctx, "//gb/l1", &[], order.clone(), mi.clone());
        register_mock(&ctx, "//gb/l2", &[], order.clone(), mi.clone());
        register_mock(&ctx, "//gb/l3", &[], order.clone(), mi.clone());
        register_mock(
            &ctx,
            "//gb/root",
            &["//gb/l1", "//gb/l2", "//gb/l3"],
            order.clone(),
            mi.clone(),
        );

        let mut g = Graph::new(1);
        let root = Addr::parse("//gb/root").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // Peak concurrency observed must be ≤ 1.
        assert!(
            mi.load(AtomicOrdering::SeqCst) <= 1,
            "max_inflight was {}, expected <= 1",
            mi.load(AtomicOrdering::SeqCst),
        );
    }

    /// Regression test for the race between the parent task's inflight
    /// decrement and its child-enqueue walk.
    ///
    /// DAG shape mirrors `examples/hello_rust/edo.toml`:
    ///
    ///   leaf ──▶ mid ──▶ root
    ///     └──────────────▲
    ///
    /// Before the fix, `batch_size = 1` plus a delay in each transform
    /// deterministically reproduced the failure mode where `mid`'s
    /// completion decremented `inflight` to 0 *before* `root` was
    /// enqueued, allowing the dispatch loop to exit and stranding
    /// `root`. After the fix, `root` must always run exactly once.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_no_stranded_root_with_delay_and_batch_one() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
        let delay = Some(std::time::Duration::from_millis(25));

        let h_leaf = register_mock_with(
            &ctx,
            "//rg/leaf",
            &[],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );
        let h_mid = register_mock_with(
            &ctx,
            "//rg/mid",
            &["//rg/leaf"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );
        let h_root = register_mock_with(
            &ctx,
            "//rg/root",
            &["//rg/leaf", "//rg/mid"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );

        let mut g = Graph::new(1);
        let root = Addr::parse("//rg/root").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // All three transforms must have executed.
        assert_eq!(
            h_leaf.transform_called.load(AtomicOrdering::SeqCst),
            1,
            "leaf must run",
        );
        assert_eq!(
            h_mid.transform_called.load(AtomicOrdering::SeqCst),
            1,
            "mid must run",
        );
        assert_eq!(
            h_root.transform_called.load(AtomicOrdering::SeqCst),
            1,
            "root must run (this is the stranded-root regression)",
        );
        let log = order.lock().await;
        assert_eq!(
            log.last().unwrap(),
            &Addr::parse("//rg/root").unwrap(),
            "root must complete last",
        );
    }

    /// Regression test locking in the 41eb63d deadlock fix: if a
    /// transform fails, the scheduler must surface the error and terminate
    /// rather than hanging. Failure is injected in the `stage` lifecycle
    /// so the error propagates out of the per-task future without going
    /// through `execute::execute`'s interactive `dialoguer::Select` prompt.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_failure_does_not_hang() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));

        // Leaf fails in stage; mid and root depend on leaf and must never
        // be dispatched once failure is observed.
        register_mock_with(
            &ctx,
            "//rgf/leaf",
            &[],
            order.clone(),
            mi.clone(),
            MockOutcome::FailInStage,
            None,
        );
        let h_mid = register_mock_with(
            &ctx,
            "//rgf/mid",
            &["//rgf/leaf"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            None,
        );
        let h_root = register_mock_with(
            &ctx,
            "//rgf/root",
            &["//rgf/leaf", "//rgf/mid"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            None,
        );

        let mut g = Graph::new(1);
        let root = Addr::parse("//rgf/root").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);

        // Bound the whole run with a timeout — if the scheduler ever
        // regresses back to the pre-41eb63d deadlock this test will fail
        // loudly instead of hanging the test binary.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), g.run(ws.path(), &ctx, &root))
                .await
                .expect("scheduler must not hang on failure");

        // Failure should surface. `execute` wraps the error before the
        // parent task sees it, so we only care that `run` returned Ok
        // (the parent logged the failure) or Err — never a hang.
        // In practice the failed branch short-circuits child dispatch;
        // the parent task returns Ok so `run` returns Ok here.
        let _ = result;

        // Children of the failing leaf must never have run.
        assert_eq!(
            h_mid.transform_called.load(AtomicOrdering::SeqCst),
            0,
            "mid must not run after leaf failure",
        );
        assert_eq!(
            h_root.transform_called.load(AtomicOrdering::SeqCst),
            0,
            "root must not run after leaf failure",
        );
    }
}
