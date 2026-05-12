//! Execution graph node state.
//!
//! A [`Node`] represents one transform vertex in the scheduler's DAG. It
//! carries enough state for the scheduler to coordinate dispatch without
//! holding a reference to the underlying [`Transform`](crate::transform::Transform):
//!
//! - **`addr`** — stable identity (used as the registry key).
//! - **`status`** — lifecycle state machine (`Pending → Running → Success|Failed`).
//! - **`id`** — content-addressed [`Id`], populated by [`Graph::fetch`](super::graph::Graph::fetch).
//! - **`cache_hit`** — whether the build cache already has an artifact for `id`.
//!
//! All mutable fields are atomics / `OnceLock` so that [`Node`] can be wrapped
//! in `Arc<Node>` and shared across worker tasks without external locking.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use crate::{context::Addr, storage::Id};

/// A single vertex in the scheduler's execution graph.
///
/// Held as `Arc<Node>` so that the dispatcher, worker tasks, and graph
/// indices can all hand out cheap clones. Mutation is lock-free via atomics.
#[derive(Debug)]
pub struct Node {
    /// Logical address of the transform this node represents.
    pub addr: Addr,
    /// Lifecycle state encoded as a `u8` matching [`NodeStatus`].
    pub status: AtomicU8,
    /// Content-addressed id for the transform, computed during
    /// [`Graph::fetch`](super::graph::Graph::fetch). Set exactly once.
    pub id: OnceLock<Id>,
    /// `true` when [`Graph::fetch`](super::graph::Graph::fetch) finds a
    /// fully-built artifact for this node in the build cache. Drives the
    /// pre-pass cascade in [`Graph::run`](super::graph::Graph::run) which
    /// short-circuits dispatch for already-built subtrees. Written once,
    /// read many times.
    pub cache_hit: AtomicBool,
}

/// Lifecycle of a [`Node`].
///
/// Transitions are strictly forward: `Pending → Running → (Success | Failed)`.
/// The numeric values are the on-wire encoding stored in [`Node::status`].
#[derive(Debug, PartialEq, Eq)]
pub enum NodeStatus {
    /// Initial state. Not yet handed to a worker.
    Pending = 0,
    /// Currently executing on a worker task.
    Running = 1,
    /// Transform completed successfully and produced an artifact.
    Success = 2,
    /// Transform failed; the scheduler will surface the first error and
    /// short-circuit dispatch of any descendants.
    Failed = 3,
}

impl From<u8> for NodeStatus {
    /// Decodes a raw status byte. Any unknown value is treated as
    /// [`NodeStatus::Failed`] — we prefer to fail loudly over silently
    /// pretending an unknown status meant success.
    fn from(value: u8) -> Self {
        match value {
            x if x == Self::Pending as u8 => Self::Pending,
            x if x == Self::Running as u8 => Self::Running,
            x if x == Self::Success as u8 => Self::Success,
            _ => Self::Failed,
        }
    }
}

impl Node {
    /// Creates a fresh node in the [`NodeStatus::Pending`] state with no
    /// cached id and no cache hit.
    pub fn new(addr: &Addr) -> Self {
        Self {
            addr: addr.clone(),
            status: AtomicU8::new(NodeStatus::Pending as u8),
            id: OnceLock::new(),
            cache_hit: AtomicBool::new(false),
        }
    }

    /// Returns `true` once the node has reached a terminal state
    /// (`Success` or `Failed`). Used by the scheduler to gate
    /// child dispatch.
    pub fn is_done(&self) -> bool {
        let status = self.status.load(Ordering::SeqCst);
        status == NodeStatus::Success as u8 || status == NodeStatus::Failed as u8
    }

    /// Marks the node as currently being executed by a worker.
    pub fn set_running(&self) {
        self.status
            .store(NodeStatus::Running as u8, Ordering::SeqCst);
    }

    /// Marks the node as successfully completed.
    pub fn set_success(&self) {
        self.status
            .store(NodeStatus::Success as u8, Ordering::SeqCst);
    }

    /// Marks the node as failed. The scheduler stops dispatching new work
    /// once any node enters this state.
    pub fn set_failed(&self) {
        self.status
            .store(NodeStatus::Failed as u8, Ordering::SeqCst);
    }

    /// Records the content-addressed id for this node. The first call wins;
    /// subsequent calls are silently ignored to keep the API forgiving when
    /// `fetch` is invoked more than once.
    pub fn set_id(&self, id: &Id) {
        if self.id.get().is_some() {
            return;
        }
        self.id.set(id.clone()).unwrap();
    }

    /// Returns the cached id if [`set_id`](Self::set_id) has been called.
    pub fn id(&self) -> Option<&Id> {
        self.id.get()
    }

    /// Records whether a fully-built artifact exists in the build cache.
    /// Called by [`Graph::fetch`](super::graph::Graph::fetch).
    pub fn set_cache_hit(&self, v: bool) {
        self.cache_hit.store(v, Ordering::SeqCst);
    }

    /// Returns `true` if the build cache already holds an artifact for this
    /// node's id.
    pub fn is_cache_hit(&self) -> bool {
        self.cache_hit.load(Ordering::SeqCst)
    }
}
