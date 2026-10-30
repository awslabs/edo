//! DAG-based planning, preparation, and bounded parallel execution.

use async_recursion::async_recursion;
use bimap::BiHashMap;
use daggy::{Dag, NodeIndex, Walker, petgraph::visit::IntoNodeReferences};
use snafu::{IntoError, OptionExt, ResultExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Index,
    path::Path,
    sync::Arc,
};
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::context::{Addr, Context, Handle, IdCache};
use crate::log::TaskStatus;
use crate::storage::{Artifact, Id};
use crate::transform::Transform;

use super::node::Node;
use super::{Result, error};

/// Lifecycle phase names recorded as the `phase` field of task events.
mod phase {
    pub const FETCH: &str = "fetch";
    pub const FETCHED: &str = "fetched";
    pub const CREATE_ENV: &str = "create-env";
    pub const SETUP: &str = "setup";
    pub const SPIN_UP: &str = "spin-up";
    pub const STAGE: &str = "stage";
    pub const EXECUTE: &str = "execute";
    pub const SPIN_DOWN: &str = "spin-down";
    pub const CLEAN: &str = "clean";
}

/// Execution DAG with per-target dependency and dispatch metadata.
///
/// ## Invariants
///
/// - For every key `k` in [`subgraphs`][Self::subgraphs], `indegrees[k]` is
///   defined and its keyset equals `subgraphs[k]`.
/// - The bimap [`index`][Self::index] is in 1-to-1 correspondence with the
///   nodes in the underlying DAG.
/// - The DAG remains acyclic: cycles surface as
///   [`SchedulerError::Graph`](super::error::SchedulerError::Graph) from
///   [`Graph::add`].
#[derive(Clone)]
pub struct Graph {
    /// Edges point from dependency to dependent.
    graph: Dag<Arc<Node>, String>,
    /// Maximum concurrent preparation and execution tasks.
    batch_size: u64,
    /// Bidirectional address-to-node index.
    index: BiHashMap<Addr, NodeIndex>,
    /// Transitive dependencies, including the target, keyed by target address.
    subgraphs: HashMap<Addr, HashSet<NodeIndex>>,
    /// Per-target in-subgraph dependency counts, cloned for each execution.
    indegrees: HashMap<Addr, HashMap<NodeIndex, u32>>,
    /// Id memoization shared across preparation and execution.
    id_cache: IdCache,
}

impl Graph {
    /// Creates an empty graph; `batch_size` must be at least one.
    pub fn new(batch_size: u64) -> Self {
        Self {
            graph: Dag::new(),
            batch_size,
            index: BiHashMap::new(),
            subgraphs: HashMap::new(),
            indegrees: HashMap::new(),
            id_cache: IdCache::new(),
        }
    }

    /// Returns the size of the reachable subgraph for `addr`, or 0 if
    /// `addr` was never added.
    pub fn subgraph_size(&self, addr: &Addr) -> usize {
        self.subgraphs.get(addr).map(|s| s.len()).unwrap_or(0)
    }

    /// Adds a transform's dependency subtree, reusing already indexed nodes.
    #[async_recursion]
    async fn add_recursive(&mut self, ctx: &Context, addr: &Addr) -> Result<NodeIndex> {
        // Deduplicate shared dependencies.
        if let Some(index) = self.index.get_by_left(addr) {
            return Ok(*index);
        }
        trace!(subsystem = "scheduler", component = "graph", op = "add-node", addr = %addr, "adding execution node");
        let transform = ctx
            .get_transform(addr)
            .context(error::ProjectTransformSnafu { addr: addr.clone() })?;
        let node_index = self.graph.add_node(Arc::new(Node::new(addr)));
        self.index.insert(addr.clone(), node_index);

        // Dependency edges are validated for cycles.
        for dep in transform.depends().await? {
            let child = self.add_recursive(ctx, &dep).await?;
            trace!(subsystem = "scheduler", component = "graph", op = "add-edge", from = %dep, to = %addr, "adding edge");
            self.graph
                .add_edge(child, node_index, format!("{dep}->{addr}"))
                .context(error::GraphSnafu)?;
        }
        Ok(node_index)
    }

    /// Adds `addr` and its dependencies, then computes target-local dispatch metadata.
    pub async fn add(&mut self, ctx: &Context, addr: &Addr) -> Result<NodeIndex> {
        let idx = self.add_recursive(ctx, addr).await?;

        // Collect dependencies via incoming edges.
        let mut subgraph: HashSet<NodeIndex> = HashSet::new();
        let mut queue: VecDeque<NodeIndex> = VecDeque::from([idx]);
        while let Some(n) = queue.pop_front() {
            if subgraph.insert(n) {
                for (_, parent) in self.graph.parents(n).iter(&self.graph) {
                    queue.push_back(parent);
                }
            }
        }

        // Count only dependencies within this target's subgraph.
        let mut indegrees: HashMap<NodeIndex, u32> = HashMap::with_capacity(subgraph.len());
        for node in &subgraph {
            let count = self
                .graph
                .parents(*node)
                .iter(&self.graph)
                .filter(|(_, p)| subgraph.contains(p))
                .count() as u32;
            indegrees.insert(*node, count);
        }

        self.subgraphs.insert(addr.clone(), subgraph);
        self.indegrees.insert(addr.clone(), indegrees);
        Ok(idx)
    }

    /// Computes ids, checks the build cache, and prepares uncached nodes concurrently.
    pub async fn fetch(&self, ctx: &Context) -> Result<()> {
        // Share transform and source ids across hashing, preparation, and execution.
        let token = ctx.cancellation().child_token();
        let ctx = ctx
            .get_handle()
            .with_cancellation(token.clone())
            .with_id_cache(self.id_cache.clone());
        let max_concurrent = self.batch_size;

        // Bound concurrent preparation without imposing dependency order.
        let semaphore = Arc::new(Semaphore::new(max_concurrent as usize));
        let mut tasks: Vec<(Addr, JoinHandle<Result<()>>)> = Vec::new();
        // Defer errors until all spawned tasks have been joined.
        let mut sync_error: Option<error::SchedulerError> = None;
        'outer: for node_ref in self.graph.node_references() {
            if token.is_cancelled() {
                break;
            }
            let node: Arc<Node> = node_ref.1.clone();
            let transform = match ctx.get(&node.addr).context(error::ProjectTransformSnafu {
                addr: node.addr.clone(),
            }) {
                Ok(t) => t,
                Err(e) => {
                    sync_error = Some(e);
                    break 'outer;
                }
            };
            // Reuse the memoized id during execution.
            let id = match ctx.unique_id_of(&node.addr).await {
                Ok(id) => id,
                Err(e) => {
                    sync_error = Some(e.into());
                    break 'outer;
                }
            };
            node.set_id(&id);

            // Only complete artifacts qualify as build-cache hits.
            match ctx.storage().find_build(&id, true).await {
                Ok(Some(_)) => {
                    info!("skipped fetch for built entry {}", node.addr);
                    node.set_cache_hit(true);
                    crate::ui_start_task!(
                        subsystem = "transform",
                        component = "scheduler",
                        op = "fetch";
                        &node.addr.to_string(),
                        crate::log::TaskStatus::Cached,
                        Some("cache-hit".into())
                    );
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    sync_error = Some(e.into());
                    break 'outer;
                }
            }
            crate::ui_start_task!(
                subsystem = "transform",
                component = "scheduler",
                op = "fetch";
                &node.addr.to_string(),
                crate::log::TaskStatus::Wait,
                Some("queued".into())
            );

            // Skip the spawn when `prepare` has nothing to do (e.g. every
            // input source is already in the local cache).
            match transform.needs_prepare(&ctx).await {
                Ok(false) => {
                    crate::ui_update_task!(
                        subsystem = "transform",
                        component = "scheduler",
                        op = "fetch";
                        &node.addr.to_string(),
                        crate::log::TaskStatus::Wait,
                        None
                    );
                    continue;
                }
                Ok(true) => {}
                Err(e) => {
                    crate::ui_update_task!(
                        subsystem = "transform",
                        component = "scheduler",
                        op = "fetch";
                        &node.addr.to_string(),
                        crate::log::TaskStatus::Failed,
                        Some(e.to_string())
                    );
                    sync_error = Some(e.into());
                    break 'outer;
                }
            }

            let task_ctx = ctx.clone();
            let node_for_task = node.clone();
            let task_token = token.clone();
            let addr_for_task = node.addr.clone();
            // Apply backpressure before spawning.
            let permit = match semaphore
                .clone()
                .acquire_owned()
                .await
                .ok()
                .context(error::InfallableSnafu)
            {
                Ok(p) => p,
                Err(e) => {
                    sync_error = Some(e);
                    crate::ui_update_task!(
                        subsystem = "transform",
                        component = "scheduler",
                        op = "fetch";
                        &node.addr.to_string(),
                        crate::log::TaskStatus::Failed,
                        None
                    );
                    break 'outer;
                }
            };
            tasks.push((
                addr_for_task.clone(),
                tokio::spawn(async move {
                    if task_token.is_cancelled() {
                        drop(permit);
                        crate::ui_update_task!(
                            subsystem = "transform",
                            component = "scheduler",
                            op = "fetch";
                            &node_for_task.addr.to_string(),
                            TaskStatus::Canceled,
                            None
                        );
                        return error::CancelledSnafu.fail();
                    }
                    let result: Result<()> = async {
                        let logf = task_ctx.log().create(format!("{id}").as_str()).await?;
                        logf.set_subject("fetch");
                        crate::ui_update_task!(
                            subsystem = "transform",
                            component = "scheduler",
                            op = "fetch";
                            &node_for_task.addr.to_string(),
                            crate::log::TaskStatus::Running,
                            Some(phase::FETCH.into())
                        );
                        transform.prepare(&logf, &task_ctx).await?;
                        Ok(())
                    }
                    .await;
                    if let Err(e) = &result {
                        task_token.cancel();
                        crate::ui_update_task!(
                            subsystem = "transform",
                            component = "scheduler",
                            op = "fetch";
                            &node_for_task.addr.to_string(),
                            crate::log::TaskStatus::Failed,
                            Some(e.to_string())
                        );
                    } else {
                        // Not terminal: the build outcome is reported by `run`.
                        crate::ui_update_task!(
                            subsystem = "transform",
                            component = "scheduler",
                            op = "fetch";
                            &node_for_task.addr.to_string(),
                            TaskStatus::Wait,
                            Some(phase::FETCHED.into())
                        );
                    }
                    drop(permit);
                    result
                }),
            ));
        }

        if sync_error.is_some() {
            token.cancel();
        }

        // Join all tasks, including failed and cancelled peers.
        let (addrs, handles): (Vec<_>, Vec<_>) = tasks.into_iter().unzip();
        let join_outcome = futures::future::join_all(handles).await;
        let mut failures: Vec<error::SchedulerError> = Vec::new();
        for (addr, joined) in addrs.into_iter().zip(join_outcome) {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // Exclude peer cancellation from root-cause errors.
                    if !matches!(e, error::SchedulerError::Cancelled) {
                        failures.push(e);
                    }
                }
                Err(join_err) => {
                    // The task died without reporting a terminal status.
                    error!(
                        subsystem = "scheduler",
                        component = "graph",
                        op = "fetch",
                        addr = %addr,
                        error = %join_err,
                        "fetch task failed"
                    );
                    crate::ui_update_task!(
                        subsystem = "transform",
                        component = "scheduler",
                        op = "fetch";
                        &addr.to_string(),
                        TaskStatus::Failed,
                        Some(join_err.to_string())
                    );
                    failures.push(error::JoinSnafu.into_error(join_err));
                }
            }
        }

        let final_error: Option<error::SchedulerError> = if let Some(e) = sync_error {
            Some(e)
        } else if !failures.is_empty() {
            Some(error::ChildSnafu { children: failures }.build())
        } else if token.is_cancelled() {
            Some(error::CancelledSnafu.build())
        } else {
            None
        };

        if let Some(e) = final_error {
            crate::ui_finish_build!();
            return Err(e);
        }
        Ok(())
    }

    /// Reports prepared nodes as successful when execution is not requested.
    pub(crate) fn finish_fetch(&self) {
        for (_, node) in self.graph.node_references() {
            if !node.is_cache_hit() {
                crate::ui_update_task!(
                    subsystem = "transform",
                    component = "scheduler",
                    op = "fetch";
                    &node.addr.to_string(),
                    TaskStatus::Success,
                    Some(phase::FETCHED.into())
                );
            }
        }
    }

    /// Executes the target subgraph in topological order with bounded concurrency.
    pub async fn run(&self, path: &Path, ctx: &Context, addr: &Addr) -> Result<()> {
        // Preserve fetch-phase ids during staging and output storage.
        let ctx_handle = ctx.get_handle().with_id_cache(self.id_cache.clone());
        let token = ctx_handle.cancellation().child_token();

        let start = self
            .index
            .get_by_left(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?;
        let root_node = self.graph.index(*start);

        // A cached target requires no dependency execution.
        if root_node.is_cache_hit() {
            crate::ui_finish_build!();
            return Ok(());
        }

        // Reset dependency counts for each execution.
        let subgraph = self
            .subgraphs
            .get(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?;
        let mut indegree = self
            .indegrees
            .get(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?
            .clone();

        // Complete cached nodes and enqueue uncached dependency-free nodes.
        let mut cascade: VecDeque<NodeIndex> = indegree
            .iter()
            .filter_map(|(n, d)| if *d == 0 { Some(*n) } else { None })
            .collect();
        let mut ready: VecDeque<NodeIndex> = VecDeque::new();
        while let Some(n) = cascade.pop_front() {
            let node = self.graph.index(n);
            if node.is_cache_hit() {
                node.set_success();
                // Release dependents within the active subgraph.
                for (_, c) in self.graph.children(n).iter(&self.graph) {
                    if !subgraph.contains(&c) {
                        continue;
                    }
                    let d = indegree.get_mut(&c).context(error::SubgraphSnafu)?;
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        cascade.push_back(c);
                    }
                }
            } else {
                ready.push_back(n);
            }
        }

        // Bound dispatch and completion queues to the worker count.
        let (work_tx, work_rx) = flume::bounded::<NodeIndex>(self.batch_size as usize);
        let (done_tx, done_rx) =
            flume::bounded::<(NodeIndex, Result<Artifact>)>(self.batch_size as usize);

        let mut workers = JoinSet::new();
        for _ in 0..self.batch_size {
            let work_rx = work_rx.clone();
            let done_tx = done_tx.clone();
            let ctx_clone = ctx_handle.clone();
            let path_buf = path.to_path_buf();
            let graph = self.graph.clone();
            let token = token.clone();
            workers.spawn(async move {
                loop {
                    let Ok(idx) = work_rx.recv_async().await else {
                        return Ok::<(), error::SchedulerError>(());
                    };
                    let node = graph.index(idx).clone();
                    let transform = match ctx_clone.get(&node.addr) {
                        Some(t) => t,
                        None => {
                            crate::ui_update_task!(
                                subsystem = "transform",
                                component = "scheduler",
                                op = "execution";
                                &node.addr,
                                TaskStatus::Failed,
                                Some("transform not registered".into())
                            );
                            let _ = done_tx
                                .send_async((
                                    idx,
                                    error::ProjectTransformSnafu {
                                        addr: node.addr.clone(),
                                    }
                                    .fail(),
                                ))
                                .await;
                            continue;
                        }
                    };
                    let id = node.id().context(error::InfallableSnafu)?.clone();
                    let log_name = format!("{id}.log");
                    let result = run_transform_lifecycle(
                        &ctx_clone, &path_buf, &node.addr, &transform, &id, &token,
                    )
                    .instrument(info_span!(
                        "transform",
                        subsystem = "scheduler",
                        addr = %node.addr,
                        log = %log_name
                    ))
                    .await;
                    if done_tx.send_async((idx, result)).await.is_err() {
                        return Ok::<(), error::SchedulerError>(());
                    }
                }
            });
        }
        drop(work_rx);
        drop(done_tx);

        // Invariants:
        //   - `inflight` == number of items posted on `work_tx` minus
        //     number of items received on `done_rx`.
        //   - `ready` only contains nodes whose indegree has reached 0 and
        //     which are not already cache hits.
        //   - Once `failed` is set or the cancellation token fires, no new
        //     work is dispatched; we drain in-flight tasks then exit.
        let mut inflight: usize = 0;
        let mut failed = false;
        let mut first_error: Option<error::SchedulerError> = None;

        'driver: loop {
            // Dispatch ready nodes until the worker limit, failure, or cancellation.
            while !ready.is_empty()
                && inflight < self.batch_size as usize
                && !failed
                && !token.is_cancelled()
            {
                let n = ready.pop_front().context(error::InfallableSnafu)?;
                self.graph.index(n).set_running();
                if let Err(e) = work_tx.try_send(n).context(error::DispatchSnafu) {
                    first_error = Some(e);
                    break 'driver;
                }
                inflight += 1;
            }

            // Without in-flight work, no further dependencies can complete.
            if inflight == 0 {
                break;
            }

            let (idx, res) = match next_completion(&done_rx, &mut workers).await {
                Ok(completion) => completion,
                Err(e) => {
                    first_error = Some(e);
                    break;
                }
            };
            inflight -= 1;
            let node = self.graph.index(idx);
            match res {
                Ok(_) => {
                    node.set_success();
                    // Release dependents only while dispatch remains active.
                    if !failed && !token.is_cancelled() {
                        for (_, c) in self.graph.children(idx).iter(&self.graph) {
                            if !subgraph.contains(&c) {
                                continue;
                            }
                            let d = indegree.get_mut(&c).context(error::InfallableSnafu)?;
                            *d = d.saturating_sub(1);
                            if *d == 0 {
                                ready.push_back(c);
                            }
                        }
                    }
                }
                Err(e) => {
                    // Preserve the first failure and drain in-flight work.
                    // The lifecycle already reported the terminal status.
                    if !matches!(e, error::SchedulerError::Cancelled) {
                        crate::ui_error!(
                            subsystem = "transform",
                            component = "scheduler",
                            op = "execution",
                            id = node.addr;
                            "{} failed: {e}",
                            node.addr
                        );
                    }
                    node.set_failed();
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    failed = true;
                    token.cancel();
                    ready.clear();
                }
            }
            // Stop dispatch on cancellation; still drain workers.
            if token.is_cancelled() {
                ready.clear();
            }
        }
        drop(work_tx);
        let mut worker_error = None;
        while let Some(result) = workers.join_next().await {
            if let Err(e) = result.context(error::JoinSnafu).and_then(|result| result) {
                worker_error.get_or_insert(e);
            }
        }
        drop(done_rx);

        if let Some(e) = worker_error {
            return Err(e);
        }

        // Report the root failure before peer cancellation.
        if let Some(e) = first_error {
            crate::ui_finish_build!();
            return Err(e);
        }
        if token.is_cancelled() {
            crate::ui_finish_build!();
            return error::CancelledSnafu.fail();
        }

        crate::ui_finish_build!();
        Ok(())
    }
}

/// Receives a completion or surfaces an exited worker's failure.
async fn next_completion(
    done: &flume::Receiver<(NodeIndex, Result<Artifact>)>,
    workers: &mut JoinSet<Result<()>>,
) -> Result<(NodeIndex, Result<Artifact>)> {
    tokio::select! {
        biased;
        result = workers.join_next() => {
            result.context(error::NoRunSnafu)?.context(error::JoinSnafu)??;
            error::NoRunSnafu.fail()
        }
        result = done.recv_async() => {
            match result {
                Ok(completion) => Ok(completion),
                Err(_) => {
                    workers.join_next().await.context(error::NoRunSnafu)?.context(error::JoinSnafu)??;
                    error::NoRunSnafu.fail()
                }
            }
        }
    }
}

/// Runs one transform and reports its terminal task status.
///
/// This is the only place an execution task's terminal status is emitted,
/// so every exit path — including early setup failures — produces exactly
/// one `success` / `failed` / `canceled` record, tagged with the last
/// stage entered.
async fn run_transform_lifecycle(
    ctx: &Handle,
    workspace: &Path,
    addr: &Addr,
    transform: &Transform,
    id: &Id,
    token: &CancellationToken,
) -> Result<Artifact> {
    let mut reached = phase::CREATE_ENV;
    let outcome =
        drive_transform_lifecycle(ctx, workspace, addr, transform, id, token, &mut reached).await;
    let status = match &outcome {
        Ok(_) => TaskStatus::Success,
        Err(error::SchedulerError::Cancelled) => TaskStatus::Canceled,
        Err(_) => TaskStatus::Failed,
    };
    crate::ui_update_task!(
        subsystem = "transform",
        component = "scheduler",
        op = "execution";
        addr,
        status,
        Some(reached.into())
    );
    outcome
}

/// Runs the transform's stages, checking cancellation between them and
/// tearing down after spinup. `reached` tracks the last stage entered;
/// teardown does not update it, so it names the stage the outcome came
/// from.
async fn drive_transform_lifecycle(
    ctx: &Handle,
    workspace: &Path,
    addr: &Addr,
    transform: &Transform,
    id: &Id,
    token: &CancellationToken,
    reached: &mut &'static str,
) -> Result<Artifact> {
    let task_id = addr.to_string();
    let task_id = task_id.as_str();
    // Remove scratch data on return.
    let temp = TempDir::new_in(workspace).context(error::TemporaryDirectorySnafu)?;
    let logf = ctx.log().create(format!("{id}").as_str()).await?;

    logf.set_subject("create-environment");
    enter_phase(task_id, reached, phase::CREATE_ENV);
    let env_addr = transform.environment().await?;
    let environment = ctx
        .create_environment(&logf, &env_addr, temp.path())
        .instrument(info_span!(
            "env-create",
            subsystem = "environment",
            addr = %addr
        ))
        .await?;
    ensure_not_cancelled(token)?;

    logf.set_subject("setup-environment");
    enter_phase(task_id, reached, phase::SETUP);
    environment.setup(&logf, ctx.storage()).await?;
    ensure_not_cancelled(token)?;

    logf.set_subject("spinup environment");
    enter_phase(task_id, reached, phase::SPIN_UP);
    environment.up(&logf).await?;

    // Every outcome after spinup must pass through teardown.
    let outcome: Result<Artifact> = async {
        ensure_not_cancelled(token)?;
        logf.set_subject("staging");
        enter_phase(task_id, reached, phase::STAGE);
        transform.stage(&logf, ctx, &environment).await?;

        ensure_not_cancelled(token)?;
        logf.set_subject("execution");
        enter_phase(task_id, reached, phase::EXECUTE);
        super::execute::execute(&logf, ctx, addr, transform, &environment).await
    }
    .await;

    // Teardown failures must not replace the build outcome.
    logf.set_subject("spindown environment");
    report_running(task_id, phase::SPIN_DOWN);
    let _ = environment.down(&logf).await;

    logf.set_subject("clean environment");
    report_running(task_id, phase::CLEAN);
    let _ = environment.clean(&logf).await;

    outcome
}

/// Records `phase` as the last stage entered and reports it as running.
fn enter_phase(task_id: &str, reached: &mut &'static str, phase: &'static str) {
    *reached = phase;
    report_running(task_id, phase);
}

/// Reports `phase` as the running stage of execution task `task_id`.
fn report_running(task_id: &str, phase: &'static str) {
    crate::ui_update_task!(
        subsystem = "transform",
        component = "scheduler",
        op = "execution";
        task_id,
        TaskStatus::Running,
        Some(phase.into())
    );
}

fn ensure_not_cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        return error::CancelledSnafu.fail();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    //! Graph tests using generated trait mocks.

    use super::*;
    use crate::context::{Addr, Context, LogVerbosity};
    use crate::environment::{Environment, Farm, MockEnvironmentImpl, MockFarmImpl};
    use crate::storage::{Artifact as StorageArtifact, Config as ArtifactConfig, Id, MediaType};
    use crate::transform::{MockTransformImpl, Transform, TransformStatus};
    use futures::FutureExt;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tempfile::TempDir;
    use tokio::sync::{Notify, OnceCell};

    #[tokio::test]
    async fn completion_surfaces_worker_error_with_another_worker_alive() {
        let (done_tx, done_rx) = flume::bounded(1);
        let mut workers = JoinSet::new();
        workers.spawn(async {
            error::PassthroughSnafu {
                message: "worker failed",
            }
            .fail()
        });
        workers.spawn(async move {
            let _sender = done_tx;
            std::future::pending::<Result<()>>().await
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_completion(&done_rx, &mut workers),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.to_string(), "worker failed");
    }

    #[tokio::test]
    async fn completion_surfaces_last_worker_panic() {
        let (done_tx, done_rx) = flume::bounded(1);
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            let _sender = done_tx;
            panic!("worker panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_completion(&done_rx, &mut workers),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(error, error::SchedulerError::Join { source } if source.is_panic()));
    }

    type SharedCtx = (Context, Arc<TempDir>);
    static SHARED: OnceCell<SharedCtx> = OnceCell::const_new();

    /// Returns the shared context, or `None` if tracing is already initialized elsewhere.
    pub(crate) async fn try_shared_context() -> Option<Context> {
        if let Some((ctx, _)) = SHARED.get() {
            return Some(ctx.clone().with_fresh_cancellation_for_test());
        }
        let dir = TempDir::new().expect("tempdir");
        match Context::init::<&Path, &Path>(
            Some(dir.path()),
            None,
            HashMap::new(),
            LogVerbosity::Info,
            crate::context::EventLog::Disabled,
        )
        .await
        {
            Ok(ctx) => {
                let _ = SHARED.set((ctx.clone(), Arc::new(dir)));
                Some(
                    SHARED
                        .get()
                        .map(|(c, _)| c.clone())
                        .unwrap_or(ctx)
                        .with_fresh_cancellation_for_test(),
                )
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

    /// Trivial pass-through environment built from `MockEnvironmentImpl`.
    /// Every method returns `Ok(())` (or a sensible default).
    fn mock_env() -> Environment {
        let mut m = MockEnvironmentImpl::new();
        m.expect_expand().returning(|p| Ok(p.to_path_buf()));
        m.expect_create_dir().returning(|_| Ok(()));
        m.expect_set_env().returning(|_, _| Ok(()));
        m.expect_get_env().returning(|_| None);
        m.expect_setup().returning(|_, _| Ok(()));
        m.expect_up().returning(|_| Ok(()));
        m.expect_down().returning(|_| Ok(()));
        m.expect_clean().returning(|_| Ok(()));
        m.expect_write_bytes().returning(|_, _| Ok(()));
        m.expect_write_stream().returning(|_, _| Ok(()));
        m.expect_unpack_stream().returning(|_, _, _| Ok(()));
        m.expect_read_bytes().returning(|_| Ok(Vec::new()));
        m.expect_read_stream().returning(|_, _| Ok(()));
        m.expect_execute().returning(|_, _, _, _| Ok(true));
        m.expect_shell().returning(|_| Ok(()));
        Environment::new(m)
    }

    pub(crate) fn mock_farm() -> Farm {
        let mut f = MockFarmImpl::new();
        f.expect_setup().returning(|_, _| Ok(()));
        f.expect_create().returning(|_, _| Ok(mock_env()));
        Farm::new(f)
    }

    /// Outcome the mock's `transform` method should return.
    #[derive(Clone)]
    pub(crate) enum MockOutcome {
        Success,
        /// Fail during staging without an interactive retry prompt.
        FailInStage,
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

    /// Creates a transform mock with lifecycle counters, ordering, and optional delay.
    #[allow(clippy::too_many_arguments)]
    fn build_mock_transform(
        addr: Addr,
        deps: Vec<Addr>,
        env_addr: Addr,
        digest: String,
        prepare_called: Arc<AtomicUsize>,
        stage_called: Arc<AtomicUsize>,
        transform_called: Arc<AtomicUsize>,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        order_log: Arc<std::sync::Mutex<Vec<Addr>>>,
        outcome: MockOutcome,
        delay: Option<std::time::Duration>,
    ) -> MockTransformImpl {
        let mut m = MockTransformImpl::new();

        {
            let env_addr = env_addr.clone();
            m.expect_environment()
                .returning(move || Ok(env_addr.clone()));
        }
        {
            let addr = addr.clone();
            let digest = digest.clone();
            m.expect_get_unique_id().returning(move |_ctx| {
                Ok(Id::builder()
                    .name(addr.to_string())
                    .digest(digest.clone())
                    .build())
            });
        }
        {
            let deps = deps.clone();
            m.expect_depends().returning(move || Ok(deps.clone()));
        }
        m.expect_needs_prepare().returning(|_ctx| Ok(true));
        {
            let prepare_called = prepare_called.clone();
            m.expect_prepare().returning(move |_log, _ctx| {
                prepare_called.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(())
            });
        }
        {
            let stage_called = stage_called.clone();
            let outcome = outcome.clone();
            m.expect_stage().returning(move |_log, _ctx, _env| {
                stage_called.fetch_add(1, AtomicOrdering::SeqCst);
                if matches!(outcome, MockOutcome::FailInStage) {
                    return Err(crate::transform::TransformError::Implementation {
                        source: Box::new(std::io::Error::other("mock stage failure")),
                    });
                }
                Ok(())
            });
        }
        {
            let transform_called = transform_called.clone();
            let inflight = inflight.clone();
            let max_inflight = max_inflight.clone();
            let order_log = order_log.clone();
            let addr_for_log = addr.clone();
            let digest_for_status = digest.clone();
            m.expect_transform().returning(move |_log, _ctx, _env| {
                transform_called.fetch_add(1, AtomicOrdering::SeqCst);
                let now = inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                // Track peak concurrency for batch-size assertions.
                max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
                order_log.lock().unwrap().push(addr_for_log.clone());
                // Widen the completion-to-dispatch race window.
                if let Some(d) = delay {
                    std::thread::sleep(d);
                }
                inflight.fetch_sub(1, AtomicOrdering::SeqCst);
                TransformStatus::Success(make_artifact(&digest_for_status))
            });
        }
        m.expect_can_shell().return_const(false);
        m.expect_shell().returning(|_env| Ok(()));
        m
    }

    /// Observable transform counters and execution order.
    #[allow(dead_code)] // fields consumed by sibling test modules
    pub(crate) struct MockHandles {
        pub addr: Addr,
        pub prepare_called: Arc<AtomicUsize>,
        pub stage_called: Arc<AtomicUsize>,
        pub transform_called: Arc<AtomicUsize>,
        pub max_inflight: Arc<AtomicUsize>,
        pub order_log: Arc<std::sync::Mutex<Vec<Addr>>>,
    }

    /// Registers a successful transform with shared order and concurrency counters.
    pub(crate) fn register_mock(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
        shared_order: Arc<std::sync::Mutex<Vec<Addr>>>,
        shared_max_inflight: Arc<AtomicUsize>,
    ) -> MockHandles {
        register_mock_with(
            ctx,
            addr_str,
            deps,
            shared_order,
            shared_max_inflight,
            MockOutcome::Success,
            None,
        )
    }

    /// Registers a transform with a configurable outcome and delay.
    pub(crate) fn register_mock_with(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
        shared_order: Arc<std::sync::Mutex<Vec<Addr>>>,
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

        let mock = build_mock_transform(
            addr.clone(),
            deps_vec,
            env_addr,
            digest,
            prepare_called.clone(),
            stage_called.clone(),
            transform_called.clone(),
            inflight,
            shared_max_inflight.clone(),
            shared_order.clone(),
            outcome,
            delay,
        );
        ctx.insert_transform_for_test(&addr, Transform::new(mock));
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight: shared_max_inflight,
            order_log: shared_order,
        }
    }

    /// Register a transform that reports no preparation work.
    pub(crate) fn register_mock_no_prepare(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
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
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::<Addr>::new()));

        let mut m = MockTransformImpl::new();
        {
            let env_addr = env_addr.clone();
            m.expect_environment()
                .returning(move || Ok(env_addr.clone()));
        }
        {
            let addr = addr.clone();
            let digest = digest.clone();
            m.expect_get_unique_id().returning(move |_ctx| {
                Ok(Id::builder()
                    .name(addr.to_string())
                    .digest(digest.clone())
                    .build())
            });
        }
        m.expect_depends().returning(move || Ok(deps_vec.clone()));
        m.expect_needs_prepare().returning(|_ctx| Ok(false));
        {
            let prepare_called = prepare_called.clone();
            m.expect_prepare().returning(move |_log, _ctx| {
                prepare_called.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(())
            });
        }
        {
            let stage_called = stage_called.clone();
            m.expect_stage().returning(move |_log, _ctx, _env| {
                stage_called.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(())
            });
        }
        {
            let transform_called = transform_called.clone();
            let digest_for_status = digest.clone();
            m.expect_transform().returning(move |_log, _ctx, _env| {
                transform_called.fetch_add(1, AtomicOrdering::SeqCst);
                TransformStatus::Success(make_artifact(&digest_for_status))
            });
        }
        m.expect_can_shell().return_const(false);
        m.expect_shell().returning(|_env| Ok(()));
        ctx.insert_transform_for_test(&addr, Transform::new(m));
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight,
            order_log: order,
        }
    }

    /// Register a transform that fails during preparation.
    pub(crate) fn register_mock_failing_prepare(
        ctx: &Context,
        addr_str: &str,
        deps: &[&str],
        peer_started: Arc<Notify>,
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
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::<Addr>::new()));

        let mut m = MockTransformImpl::new();
        {
            let env_addr = env_addr.clone();
            m.expect_environment()
                .returning(move || Ok(env_addr.clone()));
        }
        {
            let addr = addr.clone();
            let digest = digest.clone();
            m.expect_get_unique_id().returning(move |_ctx| {
                Ok(Id::builder()
                    .name(addr.to_string())
                    .digest(digest.clone())
                    .build())
            });
        }
        m.expect_depends().returning(move || Ok(deps_vec.clone()));
        m.expect_needs_prepare().returning(|_ctx| Ok(true));
        {
            let prepare_called = prepare_called.clone();
            m.expect_prepare().returning(move |_log, _ctx| {
                prepare_called.fetch_add(1, AtomicOrdering::SeqCst);
                // Adapt the synchronous mock callback without stalling Tokio workers.
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            peer_started.notified(),
                        )
                        .await
                        .expect("peer must enter prepare before failure");
                    });
                });
                Err(crate::transform::TransformError::Implementation {
                    source: Box::new(std::io::Error::other("mock prepare failure")),
                })
            });
        }
        m.expect_stage().returning(|_log, _ctx, _env| Ok(()));
        {
            let digest_for_status = digest.clone();
            m.expect_transform().returning(move |_log, _ctx, _env| {
                TransformStatus::Success(make_artifact(&digest_for_status))
            });
        }
        m.expect_can_shell().return_const(false);
        m.expect_shell().returning(|_env| Ok(()));
        ctx.insert_transform_for_test(&addr, Transform::new(m));
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight,
            order_log: order,
        }
    }

    /// Register preparation that awaits cancellation and tracks its outcome.
    pub(crate) fn register_mock_cancellable_prepare(
        ctx: &Context,
        addr_str: &str,
        bail_count: Arc<AtomicUsize>,
        completed_count: Arc<AtomicUsize>,
        started: Arc<Notify>,
    ) -> MockHandles {
        let addr = Addr::parse(addr_str).unwrap();
        let env_addr = Addr::parse("//default").unwrap();
        let digest = format!("{:064x}", fxhash(addr_str));
        let prepare_called = Arc::new(AtomicUsize::new(0));
        let stage_called = Arc::new(AtomicUsize::new(0));
        let transform_called = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::<Addr>::new()));

        let mut m = MockTransformImpl::new();
        {
            let env_addr = env_addr.clone();
            m.expect_environment()
                .returning(move || Ok(env_addr.clone()));
        }
        {
            let addr = addr.clone();
            let digest = digest.clone();
            m.expect_get_unique_id().returning(move |_ctx| {
                Ok(Id::builder()
                    .name(addr.to_string())
                    .digest(digest.clone())
                    .build())
            });
        }
        m.expect_depends().returning(|| Ok(Vec::new()));
        m.expect_needs_prepare().returning(|_ctx| Ok(true));
        {
            let prepare_called = prepare_called.clone();
            let bail = bail_count.clone();
            let done = completed_count.clone();
            m.expect_prepare().returning(move |_log, ctx| {
                prepare_called.fetch_add(1, AtomicOrdering::SeqCst);
                let token = ctx.cancellation();
                started.notify_one();
                // Adapt the synchronous mock callback without stalling Tokio workers.
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        tokio::select! {
                            _ = token.cancelled() => {
                                bail.fetch_add(1, AtomicOrdering::SeqCst);
                                Err(crate::transform::TransformError::Implementation {
                                    source: Box::new(std::io::Error::other("cancelled")),
                                })
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                done.fetch_add(1, AtomicOrdering::SeqCst);
                                Ok(())
                            }
                        }
                    })
                })
            });
        }
        m.expect_stage().returning(|_log, _ctx, _env| Ok(()));
        {
            let digest_for_status = digest.clone();
            m.expect_transform().returning(move |_log, _ctx, _env| {
                TransformStatus::Success(make_artifact(&digest_for_status))
            });
        }
        m.expect_can_shell().return_const(false);
        m.expect_shell().returning(|_env| Ok(()));
        ctx.insert_transform_for_test(&addr, Transform::new(m));
        MockHandles {
            addr,
            prepare_called,
            stage_called,
            transform_called,
            max_inflight,
            order_log: order,
        }
    }

    /// Generates stable mock digests.
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

    #[test]
    fn default_uses_batch_size_8() {
        let g = Graph::new(8);
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
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));
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
    async fn graph_fetch_skips_prepare_when_needs_prepare_false() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let h_a = register_mock_no_prepare(&ctx, "//gfnp/a", &[]);

        let mut g = Graph::new(4);
        g.add(&ctx, &Addr::parse("//gfnp/a").unwrap())
            .await
            .unwrap();
        g.fetch(&ctx).await.expect("fetch");

        assert_eq!(
            h_a.prepare_called.load(AtomicOrdering::SeqCst),
            0,
            "prepare must not be called when needs_prepare returned false"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(log_manager)]
    async fn graph_fetch_failure_emits_build_finished_and_cancels_peers() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);

        let bail = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));

        let started = Arc::new(Notify::new());
        let h_fail = register_mock_failing_prepare(&ctx, "//gfail/fail", &[], started.clone());
        let h_slow = register_mock_cancellable_prepare(
            &ctx,
            "//gfail/slow",
            bail.clone(),
            done.clone(),
            started,
        );

        register_mock_no_prepare(&ctx, "//gfail/root", &["//gfail/fail", "//gfail/slow"]);
        register_mock_no_prepare(&ctx, "//gfail/reversed", &["//gfail/slow", "//gfail/fail"]);

        for root in ["//gfail/root", "//gfail/reversed"] {
            let ctx = ctx.clone().with_fresh_cancellation_for_test();
            let mut g = Graph::new(4);
            g.add(&ctx, &Addr::parse(root).unwrap()).await.unwrap();
            let err = g.fetch(&ctx).await.expect_err("fetch must fail");
            assert!(
                matches!(err, error::SchedulerError::Child { .. }),
                "fetch should surface aggregated child error, got {err:?}",
            );
            assert!(!ctx.cancellation().is_cancelled());
        }

        assert_eq!(h_fail.prepare_called.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(h_slow.prepare_called.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(bail.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(done.load(AtomicOrdering::SeqCst), 0);

        let healthy = register_mock(
            &ctx,
            "//gfail/healthy",
            &[],
            Arc::new(std::sync::Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut graph = Graph::new(1);
        graph.add(&ctx, &healthy.addr).await.unwrap();
        graph.fetch(&ctx).await.unwrap();
        let workspace = TempDir::new().unwrap();
        graph
            .run(workspace.path(), &ctx, &healthy.addr)
            .await
            .unwrap();
        assert_eq!(healthy.transform_called.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_fetch_log_failure_cancels_queued_tasks() {
        let ctx = ctx_or_skip!();
        let first = Addr::parse("//glogfail/first").unwrap();
        let second = Addr::parse("//glogfail/second").unwrap();
        for addr in [&first, &second] {
            let mut mock = MockTransformImpl::new();
            mock.expect_depends().returning(|| Ok(Vec::new()));
            let name = addr.to_id();
            mock.expect_get_unique_id().returning(move |_| {
                Ok(Id::builder()
                    .name(name.clone())
                    .digest("a".repeat(64))
                    .build())
            });
            mock.expect_needs_prepare().returning(|_| Ok(true));
            mock.expect_prepare().never();
            ctx.insert_transform_for_test(addr, Transform::new(mock));
        }
        register_mock_no_prepare(
            &ctx,
            "//glogfail/root",
            &["//glogfail/first", "//glogfail/second"],
        );

        let log = ctx.log().create("glogfail-probe").await.unwrap();
        let id = Id::builder()
            .name(first.to_id())
            .digest("a".repeat(64))
            .build();
        let blocked_path = log.path().parent().unwrap().join(format!("{id}.log"));
        tokio::fs::create_dir(&blocked_path).await.unwrap();
        let mut g = Graph::new(1);
        g.add(&ctx, &Addr::parse("//glogfail/root").unwrap())
            .await
            .unwrap();
        let result = g.fetch(&ctx).await;
        tokio::fs::remove_dir(&blocked_path).await.unwrap();
        assert!(matches!(result, Err(error::SchedulerError::Child { .. })));
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_skips_prepare_but_executes_node_with_fetch_id() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let addr = Addr::parse("//gskip/node").unwrap();
        let artifact = make_artifact(&format!("{:064x}", fxhash("gskip")));
        let id = artifact.config().id().clone();
        let mut mock = MockTransformImpl::new();
        mock.expect_depends().returning(|| Ok(Vec::new()));
        mock.expect_get_unique_id().times(1).returning({
            let id = id.clone();
            move |_| Ok(id.clone())
        });
        mock.expect_needs_prepare()
            .times(1)
            .returning(|_| Ok(false));
        mock.expect_prepare().never();
        mock.expect_environment()
            .returning(|| Ok(Addr::parse("//default").unwrap()));
        mock.expect_stage().times(1).returning(|_, _, _| Ok(()));
        mock.expect_transform().times(1).returning({
            let addr = addr.clone();
            let id = id.clone();
            move |_, handle, _| {
                assert_eq!(
                    futures::executor::block_on(handle.unique_id_of(&addr)).unwrap(),
                    id
                );
                TransformStatus::Success(artifact.clone())
            }
        });
        ctx.insert_transform_for_test(&addr, Transform::new(mock));

        let mut graph = Graph::new(2);
        let index = graph.add(&ctx, &addr).await.unwrap();
        graph.fetch(&ctx).await.unwrap();
        assert_eq!(graph.graph.index(index).id(), Some(&id));
        assert!(!graph.graph.index(index).is_cache_hit());
        let workspace = TempDir::new().unwrap();
        graph.run(workspace.path(), &ctx, &addr).await.unwrap();
        assert_eq!(
            graph.graph.index(index).status.load(AtomicOrdering::SeqCst),
            super::super::node::NodeStatus::Success as u8
        );
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_fetch_memoizes_shared_diamond_dependency() {
        let ctx = ctx_or_skip!();
        let mut counts = Vec::new();
        for (name, deps) in [
            ("leaf", vec![]),
            ("left", vec!["leaf"]),
            ("right", vec!["leaf"]),
            ("root", vec!["left", "right"]),
        ] {
            let addr = Addr::parse(&format!("//gmemo/{name}")).unwrap();
            let deps: Vec<Addr> = deps
                .iter()
                .map(|dep| Addr::parse(&format!("//gmemo/{dep}")).unwrap())
                .collect();
            let count = Arc::new(AtomicUsize::new(0));
            counts.push(count.clone());
            let mut mock = MockTransformImpl::new();
            mock.expect_depends().returning({
                let deps = deps.clone();
                move || Ok(deps.clone())
            });
            mock.expect_get_unique_id()
                .times(1)
                .returning(move |handle| {
                    count.fetch_add(1, AtomicOrdering::SeqCst);
                    let mut hash = Sha256::new();
                    hash.update(name.as_bytes());
                    for dep in &deps {
                        let id = handle.unique_id_of(dep).now_or_never().unwrap()?;
                        hash.update(id.digest().as_bytes());
                    }
                    Ok(Id::builder()
                        .name(addr.to_id())
                        .digest(base16::encode_lower(hash.finalize().as_slice()))
                        .build())
                });
            mock.expect_needs_prepare().returning(|_| Ok(false));
            mock.expect_prepare().never();
            ctx.insert_transform_for_test(
                &Addr::parse(&format!("//gmemo/{name}")).unwrap(),
                Transform::new(mock),
            );
        }
        let mut graph = Graph::new(4);
        graph
            .add(&ctx, &Addr::parse("//gmemo/root").unwrap())
            .await
            .unwrap();
        graph.fetch(&ctx).await.unwrap();
        assert_eq!(graph.graph.node_count(), 4);
        for count in counts {
            assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        }
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_linear_chain_in_topological_order() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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

        // Entry order must match the dependency chain.
        let log = order.lock().unwrap();
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
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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

        let log = order.lock().unwrap();
        // D must be first; A must be last.
        assert_eq!(log.first().unwrap(), &Addr::parse("//gdia/d").unwrap());
        assert_eq!(log.last().unwrap(), &Addr::parse("//gdia/a").unwrap());
        assert_eq!(log.len(), 4);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_batch_size_one_serializes() {
        // Independent leaves must respect a single-worker limit.
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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

    /// Completion must enqueue dependents before deciding the run is finished.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_no_stranded_root_with_delay_and_batch_one() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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
        let log = order.lock().unwrap();
        assert_eq!(
            log.last().unwrap(),
            &Addr::parse("//rg/root").unwrap(),
            "root must complete last",
        );
    }

    /// A diamond's dependent must wait for both branches to complete.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_no_premature_root_with_diamond() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));

        let h_a = register_mock_with(
            &ctx,
            "//rgd/a",
            &[],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            None,
        );
        let h_b = register_mock_with(
            &ctx,
            "//rgd/b",
            &["//rgd/a"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            Some(std::time::Duration::from_millis(5)),
        );
        let h_c = register_mock_with(
            &ctx,
            "//rgd/c",
            &["//rgd/a"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            Some(std::time::Duration::from_millis(100)),
        );
        let h_d = register_mock_with(
            &ctx,
            "//rgd/d",
            &["//rgd/b", "//rgd/c"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            None,
        );

        let mut g = Graph::new(2);
        let root = Addr::parse("//rgd/d").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // Each node ran exactly once.
        assert_eq!(h_a.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_b.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_c.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_d.transform_called.load(AtomicOrdering::SeqCst), 1);

        // D must start after both B and C.
        let log = order.lock().unwrap();
        let pos = |a: &str| {
            log.iter()
                .position(|x| x == &Addr::parse(a).unwrap())
                .unwrap()
        };
        let pa = pos("//rgd/a");
        let pb = pos("//rgd/b");
        let pc = pos("//rgd/c");
        let pd = pos("//rgd/d");
        assert!(pa < pb && pa < pc, "A must run before B and C");
        assert!(
            pd > pb && pd > pc,
            "D must enter after both B and C (got order_log positions a={pa} b={pb} c={pc} d={pd})",
        );

        assert!(
            mi.load(AtomicOrdering::SeqCst) <= 2,
            "max_inflight was {}, expected <= 2",
            mi.load(AtomicOrdering::SeqCst),
        );
    }

    /// Staging failure must terminate dispatch without poisoning a later execution.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_failure_does_not_hang() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
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

        // Bound deadlock regressions.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            g.run(ws.path(), &ctx, &root),
        )
        .await
        .expect("scheduler must not hang on failure");

        assert!(result.is_err());
        assert!(!ctx.cancellation().is_cancelled());
        assert_eq!(h_mid.transform_called.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(h_root.transform_called.load(AtomicOrdering::SeqCst), 0);

        let leaf = register_mock(&ctx, "//rgf/leaf", &[], order.clone(), mi.clone());
        g.run(ws.path(), &ctx, &root).await.unwrap();
        assert_eq!(leaf.transform_called.load(AtomicOrdering::SeqCst), 1);

        assert_eq!(
            h_mid.transform_called.load(AtomicOrdering::SeqCst),
            1,
            "mid must run after a successful retry",
        );
        assert_eq!(
            h_root.transform_called.load(AtomicOrdering::SeqCst),
            1,
            "root must run after a successful retry",
        );
    }

    /// Shared deep dependencies must precede dependents despite wide independent fan-out.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_shared_deep_dep_respected_with_wide_root() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));

        // Widen the dispatch race window.
        let delay = Some(std::time::Duration::from_millis(10));

        // Register independent leaves before the shared dependency.
        let leaf_addrs: Vec<String> = (0..32).map(|i| format!("//bw/leaf_{i:02}")).collect();
        for a in &leaf_addrs {
            register_mock_with(
                &ctx,
                a.as_str(),
                &[],
                order.clone(),
                mi.clone(),
                MockOutcome::Success,
                delay,
            );
        }
        register_mock_with(
            &ctx,
            "//bw/glibc",
            &[],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );
        register_mock_with(
            &ctx,
            "//bw/libnv",
            &["//bw/glibc"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );
        register_mock_with(
            &ctx,
            "//bw/nvct",
            &["//bw/glibc", "//bw/libnv"],
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );

        // Traverse independent leaves before shared dependencies.
        let mut root_deps: Vec<String> = leaf_addrs.clone();
        root_deps.push("//bw/glibc".to_string());
        root_deps.push("//bw/libnv".to_string());
        root_deps.push("//bw/nvct".to_string());
        let root_dep_refs: Vec<&str> = root_deps.iter().map(|s| s.as_str()).collect();
        register_mock_with(
            &ctx,
            "//bw/root",
            &root_dep_refs,
            order.clone(),
            mi.clone(),
            MockOutcome::Success,
            delay,
        );

        let mut g = Graph::new(8);
        let root = Addr::parse("//bw/root").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // Shared dependencies must complete before dependent execution.
        let log = order.lock().unwrap();
        let pos = |a: &str| -> usize {
            log.iter()
                .position(|x| x == &Addr::parse(a).unwrap())
                .unwrap_or_else(|| panic!("{a} did not run; log = {log:?}"))
        };
        let p_glibc = pos("//bw/glibc");
        let p_libnv = pos("//bw/libnv");
        let p_nvct = pos("//bw/nvct");
        assert!(
            p_glibc < p_libnv,
            "glibc ({p_glibc}) must run before libnv ({p_libnv})",
        );
        assert!(
            p_glibc < p_nvct,
            "glibc ({p_glibc}) must run before nvct ({p_nvct})",
        );
        assert!(
            p_libnv < p_nvct,
            "libnv ({p_libnv}) must run before nvct ({p_nvct})",
        );
    }
}
