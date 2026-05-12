//! Scheduler module for DAG-based parallel transform execution.
//!
//! The scheduler is the engine's task orchestrator. Given a target
//! [`Addr`], it walks the project's transform graph, prepares each
//! transform's inputs, and executes them in topological order with a
//! bounded worker pool.
//!
//! ## Lifecycle
//!
//! A scheduler run proceeds in three phases (see [`Inner::run`]):
//!
//! 1. **Build** — [`Graph::add`](graph::Graph::add) recursively walks
//!    `addr` and its transitive dependencies, constructing a DAG of
//!    [`Node`](node::Node)s, performing transitive reduction, and
//!    pre-computing per-root indegree templates.
//! 2. **Fetch** — [`Graph::fetch`](graph::Graph::fetch) computes a
//!    content-addressed [`Id`](crate::storage::Id) for every node,
//!    queries the build cache, and downloads sources only for nodes that
//!    aren't already built. This is bounded by a [`Semaphore`] sized to
//!    `workers` to avoid hammering remote storage.
//! 3. **Run** — [`Graph::run`](graph::Graph::run) drives the actual
//!    DAG execution: ready nodes are pushed into a worker pool, completed
//!    nodes unblock their children, and cache hits are cascaded so that
//!    fully-built subtrees never spin up an environment.
//!
//! ## Concurrency model
//!
//! Worker count comes from the `[scheduler] workers` TOML key (default
//! `8`). The same number bounds:
//!
//! - the worker tasks in [`Graph::run`](graph::Graph::run),
//! - the in-flight fetch permits in [`Graph::fetch`](graph::Graph::fetch),
//! - the work/done channel capacities (so dispatch never blocks while
//!   the pool has free slots).
//!
//! All shared state ([`Node`](node::Node), the [`Graph`](graph::Graph)
//! itself) is `Arc`-wrapped and uses atomics rather than locks on the hot
//! path.
//!
//! ## Cancellation
//!
//! The scheduler cooperates with the [`Context`]'s
//! [`CancellationToken`](tokio_util::sync::CancellationToken). Workers
//! check the token between lifecycle stages, and the dispatcher refuses
//! to enqueue new work once cancellation is observed. A first failure or
//! a user-driven `quit` from [`execute::execute`] both flip the same
//! switch.

use super::context::Context;
use crate::context::{Addr, Config};
use graph::Graph;
use snafu::ResultExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::fs::create_dir_all;

/// Error types for the scheduler subsystem.
pub mod error;
/// Interactive transform executor with error recovery.
pub mod execute;
/// DAG-based execution graph for parallel transform orchestration.
pub mod graph;
/// Node representation within the scheduler execution graph.
pub mod node;

type Result<T> = std::result::Result<T, error::SchedulerError>;

/// Parallel task scheduler that builds a dependency graph and executes
/// transforms concurrently.
///
/// `Scheduler` is a thin façade over an `Arc<Inner>`; cloning is cheap and
/// safe across async tasks. The interesting orchestration lives in
/// [`graph::Graph`].
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    /// Creates a new scheduler rooted at the given workspace path.
    ///
    /// The workspace directory is the parent for per-transform temp
    /// directories created during execution; it is created on demand if
    /// it does not yet exist.
    ///
    /// Worker concurrency is read from the `[scheduler] workers` config
    /// key. Falls back to `8` when the key is missing or not an integer
    /// — we prefer a sane default over an error so misconfigured
    /// projects still build.
    pub async fn new<P: AsRef<Path>>(path: P, config: &Config) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            create_dir_all(path).await.context(error::IoSnafu)?;
        }
        // Pull `[scheduler] workers` if present; non-int values fall
        // through to the default below rather than failing the build.
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
    /// Builds the dependency graph for `addr` and executes every reachable
    /// transform.
    ///
    /// Convenience wrapper around [`Inner::run`]; see that method for the
    /// phase-by-phase walk-through.
    pub async fn run(&self, ctx: &Context, addr: &Addr) -> Result<()> {
        self.inner.run(ctx, addr).await
    }
}

/// Inner state held behind the [`Scheduler`]'s `Arc`.
struct Inner {
    /// Workspace directory used as the parent for per-transform temp
    /// directories.
    path: PathBuf,
    /// Number of concurrent worker tasks. Also bounds fetch concurrency
    /// and the work/done channel capacities.
    workers: u64,
}

impl Inner {
    /// Drives a single end-to-end build for `addr`.
    ///
    /// Sequencing matters here:
    ///
    /// 1. `Graph::new` allocates an empty DAG sized for `workers`
    ///    concurrent tasks.
    /// 2. `Graph::add` recursively pulls in `addr` and its transitive
    ///    dependencies, computes the active subgraph, and pre-builds an
    ///    indegree template for the dispatcher.
    /// 3. `Graph::fetch` populates each node's [`Id`](crate::storage::Id),
    ///    consults the build cache, and prepares (downloads sources for)
    ///    every node that isn't already built.
    /// 4. The graph is wrapped in an `Arc` (cheap; `Graph` is `Clone` but
    ///    we want shared ownership across worker tasks) and `Graph::run`
    ///    spawns the worker pool and drives the topological dispatch.
    pub async fn run(&self, ctx: &Context, addr: &Addr) -> Result<()> {
        let mut graph = Graph::new(self.workers);
        graph.add(ctx, addr).await?;
        graph.fetch(ctx).await?;
        let graph_ref = Arc::new(graph);
        graph_ref.run(&self.path, ctx, addr).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Tests for `Scheduler` construction and end-to-end execution.
    //!
    //! Re-uses the mock infrastructure from `graph::tests` since building
    //! a second, duplicate copy of the mock transform/farm/environment
    //! trio solely to exercise one extra layer of indirection would add
    //! noise without improving coverage. The `execute::tests` module is
    //! where the plan mandates mock duplication (to keep cross-module
    //! imports out of the hot path); `scheduler::tests` is adjacent to
    //! `graph::tests` in the same module tree and can share.

    use super::graph::tests::{ensure_default_farm, register_mock, try_shared_context};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    /// Build a `Config` from an inline TOML snippet. Using a real file is
    /// the only public path — `Config::load` has no in-memory constructor.
    async fn config_from_toml(dir: &TempDir, body: &str) -> Config {
        let path = dir.path().join("edo.toml");
        tokio::fs::write(&path, body).await.expect("write config");
        Config::load(Some(&path)).await.expect("parse config")
    }

    /// Empty config → `Config::load(None)` with a nonexistent file.
    async fn empty_config(dir: &TempDir) -> Config {
        let path = dir.path().join("missing.toml");
        Config::load(Some(&path)).await.expect("empty")
    }

    // ── Scheduler::new ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn new_creates_missing_workspace_dir() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("does/not/exist/yet");
        assert!(!ws.exists());
        let cfg = empty_config(&dir).await;
        Scheduler::new(&ws, &cfg)
            .await
            .expect("new should create dir");
        assert!(ws.exists(), "workspace dir must be created");
    }

    #[tokio::test]
    async fn new_defaults_to_eight_workers_when_unconfigured() {
        let dir = TempDir::new().unwrap();
        let cfg = empty_config(&dir).await;
        let s = Scheduler::new(dir.path(), &cfg).await.unwrap();
        assert_eq!(s.inner.workers, 8);
    }

    #[tokio::test]
    async fn new_respects_scheduler_workers_config() {
        let dir = TempDir::new().unwrap();
        let cfg = config_from_toml(&dir, "[scheduler]\nworkers = 3\n").await;
        let s = Scheduler::new(dir.path(), &cfg).await.unwrap();
        assert_eq!(s.inner.workers, 3);
    }

    #[tokio::test]
    async fn new_ignores_malformed_workers_key() {
        // Non-int `workers` should fall back to the default of 8, not error.
        let dir = TempDir::new().unwrap();
        let cfg = config_from_toml(&dir, "[scheduler]\nworkers = \"not-a-number\"\n").await;
        let s = Scheduler::new(dir.path(), &cfg).await.unwrap();
        assert_eq!(s.inner.workers, 8);
    }

    #[tokio::test]
    async fn new_preserves_workspace_path() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        let cfg = empty_config(&dir).await;
        let s = Scheduler::new(&ws, &cfg).await.unwrap();
        assert_eq!(s.inner.path, ws);
    }

    // ── Scheduler::run end-to-end ─────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn run_executes_linear_chain_end_to_end() {
        let Some(ctx) = try_shared_context().await else {
            eprintln!(
                "skip: global tracing subscriber already initialized by a \
                 sibling test"
            );
            return;
        };
        ensure_default_farm(&ctx);

        let order = Arc::new(TokioMutex::new(Vec::new()));
        let mi = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h_c = register_mock(&ctx, "//sr/c", &[], order.clone(), mi.clone());
        let h_b = register_mock(&ctx, "//sr/b", &["//sr/c"], order.clone(), mi.clone());
        let h_a = register_mock(&ctx, "//sr/a", &["//sr/b"], order, mi);

        let dir = TempDir::new().unwrap();
        let cfg = empty_config(&dir).await;
        let s = Scheduler::new(dir.path().join("ws"), &cfg).await.unwrap();
        s.run(&ctx, &Addr::parse("//sr/a").unwrap())
            .await
            .expect("run");

        // All three transforms ran exactly once.
        assert_eq!(h_a.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_b.transform_called.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(h_c.transform_called.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn run_propagates_missing_transform_error() {
        let Some(ctx) = try_shared_context().await else {
            eprintln!("skip");
            return;
        };
        let dir = TempDir::new().unwrap();
        let cfg = empty_config(&dir).await;
        let s = Scheduler::new(dir.path(), &cfg).await.unwrap();
        let err = s
            .run(&ctx, &Addr::parse("//nope/missing").unwrap())
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, error::SchedulerError::ProjectTransform { .. }),
            "got {err:?}",
        );
    }

    #[tokio::test]
    async fn clone_shares_inner_arc() {
        // `Scheduler::clone` must be cheap — verify the inner Arc is shared
        // rather than deep-cloned.
        let dir = TempDir::new().unwrap();
        let cfg = empty_config(&dir).await;
        let s1 = Scheduler::new(dir.path(), &cfg).await.unwrap();
        let s2 = s1.clone();
        assert!(Arc::ptr_eq(&s1.inner, &s2.inner));
    }
}
