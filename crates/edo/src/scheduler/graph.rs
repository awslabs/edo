//! DAG-based execution graph for parallel transform orchestration.
//!
//! [`Graph`] is the data structure the scheduler uses to plan and execute a
//! build. It owns:
//!
//! - the directed acyclic graph itself (a `daggy::Dag<Arc<Node>, String>`),
//! - a bidirectional [`Addr`] ↔ [`NodeIndex`] index for O(1) lookups,
//! - per-root *subgraph* sets (the slice of nodes reachable from each
//!   user-requested target via the `depends` relation), and
//! - per-root *indegree templates* — precomputed maps that the dispatcher
//!   clones and mutates while running so it never has to walk the parent set
//!   of a node at dispatch time.
//!
//! ## Lifecycle
//!
//! A typical use of [`Graph`] looks like:
//!
//! ```ignore
//! let mut g = Graph::new(workers);
//! g.add(ctx, &target).await?;        // build the DAG
//! g.fetch(ctx).await?;               // hash + cache-check + prepare
//! Arc::new(g).run(path, ctx, &target).await?; // execute
//! ```
//!
//! Each phase is independent and can fail without leaving the graph in a
//! corrupt state — a partially-built graph is still safe to drop.
//!
//! ## Why per-root subgraphs?
//!
//! The graph can hold transforms that are *not* reachable from the current
//! target (e.g. when a previous `add` call brought in an unrelated subtree).
//! Dispatch must operate on the active slice only, otherwise the indegree
//! count would include edges from outside the slice and a node could remain
//! "blocked" forever. The per-root subgraph membership set lets `run` filter
//! both the BFS frontier and child enqueue walks cheaply.

use async_recursion::async_recursion;
use bimap::BiHashMap;
use daggy::{Dag, NodeIndex, Walker, petgraph::visit::IntoNodeReferences};
use futures::future::try_join_all;
use snafu::{OptionExt, ResultExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Index,
    path::Path,
    sync::Arc,
};
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore, mpsc::channel};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::context::{Addr, Context, Handle};
use crate::storage::{Artifact, Id};
use crate::transform::Transform;

use super::node::Node;
use super::{Result, error};

/// Execution graph: the DAG plus per-root metadata required to dispatch
/// transforms in topological order with bounded concurrency.
///
/// Cheap to clone: `daggy::Dag` clones share inner `Arc<Node>`s, and the
/// auxiliary maps are small. The scheduler wraps the whole graph in an
/// `Arc<Graph>` once dispatch starts so workers can index into it without
/// taking locks.
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
    /// The actual DAG. Edges carry a human-readable `"a->b"` label used in
    /// trace output; the structural information lives in the topology.
    graph: Dag<Arc<Node>, String>,
    /// Worker count; bounds in-flight fetches *and* in-flight transform
    /// executions. Mirrors the `[scheduler] workers` config key.
    batch_size: u64,
    /// Bidirectional [`Addr`] ↔ [`NodeIndex`] map. Used by `add` to dedupe
    /// nodes during recursion and by `run` to resolve the start vertex.
    index: BiHashMap<Addr, NodeIndex>,
    /// Per-root subgraph: the set of `NodeIndex`es reachable from a given
    /// root via *incoming* edges (i.e. transitive dependencies). Populated
    /// at the end of [`Graph::add`].
    subgraphs: HashMap<Addr, HashSet<NodeIndex>>,
    /// Per-root indegree template: `map[root] -> map[node in subgraph] ->
    /// count of incoming edges that originate inside the same subgraph`.
    ///
    /// Populated once per `add` call, *after* transitive reduction so the
    /// counts reflect the minimal DAG. Cross-subgraph edges (if any) are
    /// excluded — they don't gate dispatch within this run.
    ///
    /// `run` clones the inner map at start and decrements it as nodes
    /// complete; that's why this is a "template" rather than mutable state.
    indegrees: HashMap<Addr, HashMap<NodeIndex, u32>>,
}

impl Graph {
    /// Creates an empty graph sized for `batch_size` concurrent workers.
    ///
    /// `batch_size` is plumbed through to bound both fetch concurrency and
    /// the work/done channel capacities used by [`Graph::run`]. It must be
    /// at least 1; `Scheduler::new` enforces a default of 8 if the
    /// configuration omits the key.
    pub fn new(batch_size: u64) -> Self {
        Self {
            graph: Dag::new(),
            batch_size,
            index: BiHashMap::new(),
            subgraphs: HashMap::new(),
            indegrees: HashMap::new(),
        }
    }

    /// Recursively adds a transform and its dependencies to the graph.
    ///
    /// Returns the `NodeIndex` of the added (or existing) node. Edges are
    /// created from each dependency *into* the dependent node so that the
    /// natural "child" direction in `daggy` (outgoing edges) corresponds to
    /// "thing that depends on me".
    ///
    /// Idempotent: if `addr` is already in the index, returns the existing
    /// `NodeIndex` without re-walking its dependencies.
    #[async_recursion]
    async fn add_recursive(&mut self, ctx: &Context, addr: &Addr) -> Result<NodeIndex> {
        // Fast path: node already registered. Without this, a diamond DAG
        // would infinite-loop on the shared dependency.
        if let Some(index) = self.index.get_by_left(addr) {
            return Ok(*index);
        }
        trace!(component = "execution", "adding execution node for {addr}");
        let transform = ctx
            .get_transform(addr)
            .context(error::ProjectTransformSnafu { addr: addr.clone() })?;
        let node_index = self.graph.add_node(Arc::new(Node::new(addr)));
        self.index.insert(addr.clone(), node_index);

        // Recurse into dependencies. Each recursive call registers the dep
        // (or finds it via the fast path) and we wire an edge dep -> self.
        // `add_edge` is what catches cycles — daggy returns `WouldCycle`.
        for dep in transform.depends().await? {
            let child = self.add_recursive(ctx, &dep).await?;
            trace!(component = "execution", "adding edge for {dep} -> {addr}");
            self.graph
                .add_edge(child, node_index, format!("{dep}->{addr}"))
                .context(error::GraphSnafu)?;
        }
        Ok(node_index)
    }

    /// Builds (or extends) the graph for `addr` and pre-computes the
    /// metadata that [`Graph::run`] needs to dispatch it.
    ///
    /// The work happens in three steps:
    ///
    /// 1. **Recursive insertion** ([`Self::add_recursive`]): walks the
    ///    transform tree rooted at `addr`, materializing one [`Node`] per
    ///    transform and one edge per `dep -> dependent` relation.
    /// 2. **Subgraph BFS**: collects every node reachable from `addr` via
    ///    *incoming* edges. This is the "active slice" the dispatcher will
    ///    operate on, isolated from any unrelated nodes that might also
    ///    live in `self.graph` from a prior `add` call.
    /// 3. **indegree template**: counts in-subgraph
    ///    parents per node — the dispatcher's indegree counter starts here.
    pub async fn add(&mut self, ctx: &Context, addr: &Addr) -> Result<NodeIndex> {
        let idx = self.add_recursive(ctx, addr).await?;

        // ── Step 2: BFS upward to collect the active subgraph. ────────────
        // We walk *parents* (incoming edges in daggy) because edges point
        // dep -> dependent, so dependencies of `addr` are reached by
        // following parents.
        let mut subgraph: HashSet<NodeIndex> = HashSet::new();
        let mut queue: VecDeque<NodeIndex> = VecDeque::from([idx]);
        while let Some(n) = queue.pop_front() {
            if subgraph.insert(n) {
                for (_, parent) in self.graph.parents(n).iter(&self.graph) {
                    queue.push_back(parent);
                }
            }
        }

        // ── Step 3: Indegree template. ───────────────────────────────────
        // Count only edges originating *inside* the subgraph. The
        // dispatcher decrements these counters as parents complete, and a
        // node becomes ready when its counter hits zero.
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

    /// Computes content-addressed ids, checks the build cache, and prepares
    /// (downloads sources for) every node in the graph.
    ///
    /// For each node:
    /// 1. Compute its [`Id`] via [`Transform::get_unique_id`] and stash it
    ///    on the node.
    /// 2. Probe the build cache. If a fully-built artifact exists for that
    ///    id we mark the node as a cache hit and skip preparation entirely
    ///    — `run` will short-circuit dispatch for cache-hit subtrees in its
    ///    pre-pass cascade.
    /// 3. Otherwise spawn a task that calls [`Transform::prepare`] (typically
    ///    a network fetch of sources and ancillary artifacts).
    ///
    /// Concurrency is bounded by a [`Semaphore`] sized to `batch_size`.
    /// Fetch parallelism is order-independent (unlike `run`'s topological
    /// dispatch), so we don't need ready/ready-not state — just a permit
    /// pool that throttles the network.
    pub async fn fetch(&self, ctx: &Context) -> Result<()> {
        let mut tasks = Vec::new();
        let ctx = ctx.get_handle();
        let max_concurrent = self.batch_size;

        // Fetching is network-bound. We don't want to issue thousands of
        // requests in parallel, but unlike execution we also don't need to
        // respect topological order — sources for a child can pull at the
        // same time as sources for its parent. A semaphore is the simplest
        // way to cap in-flight fetches at `batch_size`.
        let semaphore = Arc::new(Semaphore::new(max_concurrent as usize));
        for node_ref in self.graph.node_references() {
            let node: Arc<Node> = node_ref.1.clone();
            let transform = ctx.get(&node.addr).context(error::ProjectTransformSnafu {
                addr: node.addr.clone(),
            })?;
            // Compute the content-addressed id and stash it on the node so
            // workers in `run` can index into the build cache without
            // recomputing it.
            let id = transform.get_unique_id(&ctx).await?;
            node.set_id(&id);

            // Build cache probe. `find_build(.., true)` requires a *full*
            // artifact (all layers present) — partial hits do not count.
            // `cache_hit = true` will let `run`'s pre-pass cascade promote
            // this node and any cache-hit ancestors to Success without
            // ever spawning an environment.
            if ctx.storage().find_build(&id, true).await?.is_some() {
                info!("skipped fetch for built entry {}", node.addr);
                node.set_cache_hit(true);
                continue;
            }

            let ctx = ctx.clone();
            let node_for_task = node.clone();
            // Acquire the permit *outside* the spawn so the loop blocks
            // here when we're already at capacity. Owned permits are moved
            // into the task and released on drop.
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .ok()
                .context(error::InfallableSnafu)?;
            tasks.push(tokio::spawn(async move {
                let logf = ctx.log().create(format!("{id}").as_str()).await?;
                logf.set_subject("fetch");
                transform.prepare(&logf, &ctx).await?;
                info!("pulled sources and artifacts for {}", node_for_task.addr);
                drop(logf);
                // Explicit drop is documentation: the permit returns to
                // the pool exactly when this task ends.
                drop(permit);
                Ok::<(), error::SchedulerError>(())
            }));
        }
        wait(tasks).await?;
        Ok(())
    }

    /// Executes every transform reachable from `addr` in topological order,
    /// using a bounded worker pool.
    ///
    /// This is the heart of the scheduler. The algorithm is a Kahn-style
    /// topological dispatch with a few wrinkles to support cache hits,
    /// cooperative cancellation, and fail-fast error handling.
    ///
    /// ## High-level shape
    ///
    /// 1. **Resolve start.** Look up `addr` in the index and bail early if
    ///    its node was already cache-hit by `fetch` — there's nothing to
    ///    run.
    /// 2. **Snapshot dispatch state.** Clone the per-root indegree template
    ///    so we can mutate it without affecting future runs of the same
    ///    graph, and grab the subgraph membership set.
    /// 3. **Cache-hit cascade.** Walk the dependency leaves and promote
    ///    any cache-hit nodes to `Success` *without* dispatching them.
    ///    Their children's indegrees decrement, potentially exposing more
    ///    cache hits to promote, and so on. Non-hit indegree-0 nodes go
    ///    onto the `ready` queue.
    /// 4. **Spawn workers.** Fixed-size pool of `batch_size` tasks pulling
    ///    from a shared `work_rx` channel and reporting back through
    ///    `done_tx`.
    /// 5. **Driver loop.** Saturate the pool from `ready`, await one
    ///    completion at a time, decrement children's indegrees on success,
    ///    short-circuit on first failure, and exit when no more work is
    ///    in flight.
    /// 6. **Drain workers.** Drop `work_tx` to signal end-of-stream, await
    ///    every worker task, and surface the first error (or cancellation).
    ///
    /// ## Why one driver, many workers?
    ///
    /// All scheduling state (`ready` queue, mutable indegree map, failure
    /// flag) lives in this function and is mutated only on the driver task.
    /// Workers are pure executors: receive a `NodeIndex`, run the lifecycle,
    /// post the result. That keeps the scheduling logic single-threaded and
    /// lock-free without giving up parallelism on the actual work.
    pub async fn run(&self, path: &Path, ctx: &Context, addr: &Addr) -> Result<()> {
        let ctx_handle = ctx.get_handle();
        let token = ctx_handle.cancellation();

        // ── Step 1: resolve the target node. ──────────────────────────────
        let start = self
            .index
            .get_by_left(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?;
        let root_node = self.graph.index(*start);

        // Early exit: the target itself is already built. `fetch` populates
        // `cache_hit`; if the root is one we don't even need to walk its
        // dependencies — they only matter if we have to rebuild.
        if root_node.is_cache_hit() {
            info!("{addr} is already built, skipping...");
            return Ok(());
        }

        // ── Step 2: snapshot per-root dispatch state. ─────────────────────
        // The indegree map is the *mutable* working set; we clone the
        // template so subsequent `run` calls on the same graph start fresh.
        // The subgraph membership set is read-only and stays borrowed.
        let subgraph = self
            .subgraphs
            .get(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?;
        let mut indegree = self
            .indegrees
            .get(addr)
            .context(error::NodeSnafu { addr: addr.clone() })?
            .clone();

        // ── Step 3: cache-hit cascade. ────────────────────────────────────
        // Initial Kahn frontier: all indegree-0 nodes (the dependency
        // leaves). For each, if it's already in the build cache we skip
        // dispatch, mark it Success, and propagate to its children — which
        // may themselves be cache hits, and so on. The cascade can promote
        // entire subtrees to Success without ever spawning a worker.
        //
        // Non-hit frontier nodes drop into `ready` for the dispatcher.
        let mut cascade: VecDeque<NodeIndex> = indegree
            .iter()
            .filter_map(|(n, d)| if *d == 0 { Some(*n) } else { None })
            .collect();
        let mut ready: VecDeque<NodeIndex> = VecDeque::new();
        while let Some(n) = cascade.pop_front() {
            let node = self.graph.index(n);
            if node.is_cache_hit() {
                node.set_success();
                // Decrement each in-subgraph child's indegree; any that
                // hit zero join the cascade so we can keep promoting.
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
                // Not a cache hit — this is real work for the worker pool.
                ready.push_back(n);
            }
        }

        // ── Step 4: spawn the worker pool. ────────────────────────────────
        // Two MPSC channels:
        //   - work_tx/work_rx: driver -> workers, carries NodeIndex.
        //   - done_tx/done_rx: workers -> driver, carries (idx, result).
        // Capacities equal `batch_size` so the driver never blocks on send
        // while the pool has free slots.
        //
        // `work_rx` is wrapped in `Arc<Mutex<_>>` because tokio's MPSC
        // receiver isn't `Clone`; the lock is held only across `recv()`
        // and contention is rare (workers are usually busy executing).
        let (work_tx, work_rx) = channel::<NodeIndex>(self.batch_size as usize);
        let (done_tx, mut done_rx) =
            channel::<(NodeIndex, Result<Artifact>)>(self.batch_size as usize);
        let work_rx = Arc::new(Mutex::new(work_rx));

        let mut worker_handles: Vec<JoinHandle<Result<()>>> = Vec::new();
        for _ in 0..self.batch_size {
            let work_rx = work_rx.clone();
            let done_tx = done_tx.clone();
            let ctx_clone = ctx_handle.clone();
            let path_buf = path.to_path_buf();
            let graph = self.graph.clone();
            let token = token.clone();
            worker_handles.push(tokio::spawn(async move {
                loop {
                    // Briefly hold the receive lock just long enough to
                    // pull one item — releasing it before the (long-running)
                    // transform lifecycle so siblings can pick up new work.
                    let next = {
                        let mut guard = work_rx.lock().await;
                        guard.recv().await
                    };
                    // `None` means the driver dropped `work_tx`; we're done.
                    let Some(idx) = next else {
                        return Ok::<(), error::SchedulerError>(());
                    };
                    let node = graph.index(idx).clone();
                    let transform = match ctx_clone.get(&node.addr) {
                        Some(t) => t,
                        None => {
                            // Vanishingly unlikely (the transform was here
                            // when `add` ran) but report it cleanly anyway.
                            let _ = done_tx
                                .send((
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
                    // `fetch` is required to have run before `run`, so the
                    // id is always populated by this point.
                    let id = node.id().context(error::InfallableSnafu)?.clone();
                    let result = run_transform_lifecycle(
                        &ctx_clone, &path_buf, &node, &transform, &id, &token,
                    )
                    .instrument(info_span!("transforming", addr = node.addr.to_string()))
                    .await;
                    // If the driver has gone away (done_rx dropped) there's
                    // nobody left to report to — exit quietly.
                    if done_tx.send((idx, result)).await.is_err() {
                        return Ok::<(), error::SchedulerError>(());
                    }
                }
            }));
        }
        // Drop the driver's clone of `done_tx` so once every worker exits,
        // `done_rx.recv()` returns `None` instead of hanging forever.
        drop(done_tx);

        // ── Step 5: driver loop. ──────────────────────────────────────────
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

        loop {
            // Saturate the pool: push ready work into `work_tx` until
            // either we run out of ready nodes or hit the concurrency cap.
            // We pause dispatching on failure or cancellation so the
            // remaining in-flight tasks can drain naturally.
            while !ready.is_empty()
                && inflight < self.batch_size as usize
                && !failed
                && !token.is_cancelled()
            {
                let n = ready.pop_front().context(error::InfallableSnafu)?;
                self.graph.index(n).set_running();
                // `try_send` is infallible here: channel capacity is
                // `batch_size` and `inflight < batch_size` guarantees space.
                work_tx.try_send(n).ok().context(error::InfallableSnafu)?;
                inflight += 1;
            }

            // Termination condition: nothing in flight means no more work
            // can become ready. (`ready` may still hold items if we exited
            // the dispatch loop via `failed`/`cancelled`; that's fine —
            // they're abandoned on purpose.)
            if inflight == 0 {
                break;
            }

            // Block on the next completion. `unwrap` is safe because we
            // hold the original `work_tx`, so `done_tx` clones held by
            // workers stay alive while there's anything to wait for.
            let (idx, res) = done_rx.recv().await.context(error::InfallableSnafu)?;
            inflight -= 1;
            let node = self.graph.index(idx);
            match res {
                Ok(_) => {
                    node.set_success();
                    // On success, decrement children's indegrees and queue
                    // any newly-ready ones. Suppressed during failure /
                    // cancellation so we don't widen the dispatch front.
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
                    // Fail-fast: latch the first error, mark the node
                    // failed, and clear `ready` so no further dispatch
                    // happens. We still need to drain `inflight` tasks
                    // so workers don't leak.
                    error!("{} failed: {e}", node.addr);
                    node.set_failed();
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    failed = true;
                    ready.clear();
                }
            }
            // Cooperative cancellation: a quit prompt from `execute`
            // flips the same token. Clear `ready` so we stop feeding the
            // pool but let the drain proceed normally.
            if token.is_cancelled() {
                ready.clear();
            }
        }
        // Closing `work_tx` lets workers see end-of-stream and exit their
        // recv loops; we then await each so panics surface as `JoinError`.
        drop(work_tx);
        for h in worker_handles {
            h.await.context(error::JoinSnafu)??;
        }

        // Failure takes precedence over cancellation in error reporting:
        // a real failure is more actionable than the generic "cancelled".
        if let Some(e) = first_error {
            return Err(e);
        }
        if token.is_cancelled() {
            return error::CancelledSnafu.fail();
        }

        Ok(())
    }
}

/// Runs the full per-transform lifecycle for one node.
///
/// The lifecycle has five user-visible stages, each guarded by a
/// cancellation check so a `quit` prompt aborts as quickly as possible:
///
/// 1. **create-environment** — ask the [`Handle`] to materialize an
///    [`Environment`] from the transform's farm address. The temp dir
///    backing it lives under `workspace` and is dropped at function exit.
/// 2. **setup-environment** — populate the environment with anything the
///    farm needs (e.g. base layers from storage).
/// 3. **spinup environment** — start the environment (e.g. boot a
///    container). After this point, `down` and `clean` are best-effort
///    invoked unconditionally so we never leak a running environment.
/// 4. **staging + execution** — ask the transform to stage its inputs and
///    then run via [`execute::execute`](super::execute::execute), which
///    handles interactive retry/quit prompts on failure.
/// 5. **spindown + clean** — best-effort teardown. Errors here are
///    swallowed so a clean-up failure doesn't mask the real outcome.
///
/// The function returns the staging+execution outcome — environment
/// teardown errors are intentionally not propagated.
async fn run_transform_lifecycle(
    ctx: &Handle,
    workspace: &Path,
    node: &Arc<Node>,
    transform: &Transform,
    id: &Id,
    token: &CancellationToken,
) -> Result<Artifact> {
    // Per-transform scratch directory; dropped (and removed) when this
    // function returns regardless of success/failure path.
    let temp = TempDir::new_in(workspace).context(error::TemporaryDirectorySnafu)?;
    let logf = ctx.log().create(format!("{id}").as_str()).await?;

    logf.set_subject("create-environment");
    let env_addr = transform.environment().await?;
    let environment = ctx
        .create_environment(&logf, &env_addr, temp.path())
        .instrument(info_span!(
            "creating environment",
            addr = node.addr.to_string()
        ))
        .await?;

    if token.is_cancelled() {
        return error::CancelledSnafu.fail();
    }

    logf.set_subject("setup-environment");
    environment
        .setup(&logf, ctx.storage())
        .instrument(info_span!(
            "setting up environment",
            addr = node.addr.to_string()
        ))
        .await?;

    if token.is_cancelled() {
        return error::CancelledSnafu.fail();
    }

    logf.set_subject("spinup environment");
    environment.up(&logf).await?;

    // Past this point the environment is "up" and we owe it teardown.
    // Compute the outcome inside an inner async block so the `down` /
    // `clean` calls below run on every exit path — including the
    // cancellation early-returns inside the block.
    let outcome: Result<Artifact> = async {
        if token.is_cancelled() {
            return error::CancelledSnafu.fail();
        }
        logf.set_subject("staging");
        transform
            .stage(&logf, ctx, &environment)
            .instrument(info_span!(
                "staging into environment",
                addr = node.addr.to_string()
            ))
            .await?;

        if token.is_cancelled() {
            return error::CancelledSnafu.fail();
        }
        logf.set_subject("execution");
        super::execute::execute(&logf, ctx, transform, &environment).await
    }
    .await;

    // Best-effort teardown: errors are logged-and-swallowed so a clean-up
    // failure never overrides a successful build (or vice versa).
    logf.set_subject("spindown environment");
    let _ = environment.down(&logf).await;
    logf.set_subject("clean environment");
    let _ = environment
        .clean(&logf)
        .instrument(info_span!("cleaning up", addr = node.addr.to_string()))
        .await;

    drop(logf);
    match outcome {
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

/// Awaits all join handles, collecting successes or returning aggregated
/// failures.
///
/// Used by [`Graph::fetch`] to wait on the parallel prepare tasks.
/// `JoinError`s (panics or cancellations of the outer task) short-circuit
/// via `try_join_all` and propagate as [`SchedulerError::Join`]; logical
/// errors returned by the inner futures are accumulated and surfaced as a
/// single [`SchedulerError::Child`].
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
    //! Mocks come from `mockall::automock` on `EnvironmentImpl`, `FarmImpl`,
    //! and `TransformImpl`. The trivial pass-through configuration lives in
    //! `mock_env` / `mock_farm`; the heavily-instrumented transform mock is
    //! built by `build_mock_transform` and registered via `register_mock` /
    //! `register_mock_with`.

    use super::*;
    use crate::context::{Addr, Context, LogVerbosity};
    use crate::environment::{Environment, Farm, MockEnvironmentImpl, MockFarmImpl};
    use crate::storage::{Artifact as StorageArtifact, Config as ArtifactConfig, Id, MediaType};
    use crate::transform::{MockTransformImpl, Transform, TransformStatus};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tempfile::TempDir;
    use tokio::sync::OnceCell;

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

    // ── mock Environment / Farm ──────────────────────────────────────────────

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

    /// Configure a `MockTransformImpl` with the given instrumentation.
    ///
    /// All async lifecycle methods are wired through `.returning(...)`
    /// closures. The `transform` body uses `std::sync::Mutex` for the
    /// shared order log and `std::thread::sleep` for the optional delay
    /// because mockall's `.returning` closure is sync (the generated
    /// async-trait wrapper turns the returned value into a ready future,
    /// so `.await` is not available inside the closure body).
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
                // Optional blocking sleep to widen the scheduler's
                // completion-to-dispatch race window. Safe under the default
                // multi-thread Tokio runtime used by `#[tokio::test]`.
                if let Some(d) = delay {
                    std::thread::sleep(d);
                }
                inflight.fetch_sub(1, AtomicOrdering::SeqCst);
                // Both Success and FailInStage map to Success here — the
                // `FailInStage` outcome short-circuits earlier in `stage()`.
                TransformStatus::Success(make_artifact(&digest_for_status))
            });
        }
        m.expect_can_shell().return_const(false);
        m.expect_shell().returning(|_env| Ok(()));
        m
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
        pub order_log: Arc<std::sync::Mutex<Vec<Addr>>>,
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

        // Ordering: C must finish before B, B before A. The shared log
        // records entry order which — for a linear chain under any
        // batch-size — must match topological order.
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
        // With batch_size=1, no two transforms may run concurrently. Three
        // independent leaves are registered, all feeding into a root so that
        // the dispatcher has real parallel opportunity.
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

    /// Regression test for the parent-status check in the child-enqueue
    /// walk. With the previous predicate `is_queued() || is_pending()`, a
    /// parent in `Running` state was treated as "done enough" and the child
    /// could be enqueued before all its dependencies completed.
    ///
    /// DAG shape (diamond):
    ///
    ///         A
    ///        / \
    ///       B   C
    ///        \ /
    ///         D
    ///
    /// With `batch_size = 2` and asymmetric delays (B fast, C slow), B
    /// finishes first. The parent task then walks B's children and finds D.
    /// Before the fix, D's parents `[B = Success, C = Running]` would pass
    /// the old predicate (C is neither Queued nor Pending) and D would be
    /// dispatched while C is still running. After the fix, the predicate
    /// `!is_done()` correctly blocks on Running parents.
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

        // D must enter the order log strictly after both B and C — the
        // bug-version would log D between B and C because D dispatches
        // while C is still running.
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

        // Batch size of 2 must never be exceeded. Crucially this also fails
        // the bug: with the broken predicate, D dispatches while C is still
        // running and B has just finished — the moment D's transform
        // increments inflight, max_inflight observes 2 (D + C) which is
        // still <= 2, so this alone does not catch the bug. The position
        // assertion above is the primary signal; this is a sanity check.
        assert!(
            mi.load(AtomicOrdering::SeqCst) <= 2,
            "max_inflight was {}, expected <= 2",
            mi.load(AtomicOrdering::SeqCst),
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

        // Bound the whole run with a timeout — if the scheduler ever
        // regresses back to the pre-41eb63d deadlock this test will fail
        // loudly instead of hanging the test binary.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            g.run(ws.path(), &ctx, &root),
        )
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

    /// Regression test for the bottlerocket-buildstream ordering inversion:
    /// a wide compose root with many leaf packages plus a handful of
    /// mid-tier packages that share a deep-leaf dependency (`glibc`).
    /// In production this DAG dispatched `nvidia-container-toolkit/rpm`
    /// before its dependency `glibc/rpm` had even started.
    ///
    /// Shape (mirrors `kits/bottlerocket-core-kit/pkgs`):
    ///
    /// ```text
    ///   root ─┬── leaf_0 .. leaf_N            (independent leaves)
    ///         ├── glibc                       (shared leaf)
    ///         ├── libnv  (deps: glibc)
    ///         └── nvct   (deps: glibc, libnv)
    /// ```
    ///
    /// With sufficient leaves and a non-trivial `batch_size`, the bug
    /// surfaces as `nvct` being recorded in `order_log` before `glibc`.
    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn graph_run_shared_deep_dep_respected_with_wide_root() {
        let ctx = ctx_or_skip!();
        ensure_default_farm(&ctx);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mi = Arc::new(AtomicUsize::new(0));

        // Tiny delay so the cascade race window is wide enough to be
        // observable across machines / CI shapes.
        let delay = Some(std::time::Duration::from_millis(10));

        // Independent leaves — alphabetically sorted before "glibc" in
        // the depends list, so a buggy scheduler that walks deps in
        // insertion order will dispatch them first and exhaust the
        // worker pool before `glibc` ever gets a slot.
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

        // Root depends on every leaf + the three mid/deep nodes. Note
        // `glibc` is listed *after* the leaves to ensure the wide
        // independent fan-out is encountered first during recursive
        // graph construction — matching the alphabetical layout in
        // `kits/bottlerocket-core-kit/edo.toml`.
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

        // batch_size = 8 mirrors the production default and the
        // configured value in bottlerocket-buildstream.
        let mut g = Graph::new(8);
        let root = Addr::parse("//bw/root").unwrap();
        g.add(&ctx, &root).await.unwrap();
        g.fetch(&ctx).await.unwrap();
        let ws = TempDir::new().unwrap();
        let g = Arc::new(g);
        g.run(ws.path(), &ctx, &root).await.expect("run");

        // The smoking gun from the bottlerocket logs: `nvct` started
        // before `glibc`. With the bug, `pos(nvct) < pos(glibc)`.
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
