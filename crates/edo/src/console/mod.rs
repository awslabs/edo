//! Console subsystem: structured build-event stream + renderers.
//!
//! ## Roles
//!
//! Three orthogonal concerns share this module:
//!
//! - **Per-task `.log` files** (in [`Log`](super::context::Log)) capture
//!   raw stdout/stderr per transform. Untouched by the console.
//! - **`tracing` events** flow into a JSON Lines file under `.edo/logs/`
//!   (`edo.jsonl`) via [`LogManager`](super::context::LogManager). They are
//!   no longer the source of truth for progress.
//! - **`ConsoleEvent`s** (this module) are the typed event channel the
//!   scheduler / transforms publish to. Sinks (canvas renderer, JSONL
//!   file, simple stderr) consume them.
//!
//! ## Phasing note
//!
//! Phase 3 ships the inline ratatui canvas via [`render::spawn_render_task`]
//! and [`Console::install_canvas`]. Failure prompts run inside the same
//! render task — see [`Console::prompt`].

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

pub mod event;
pub mod render;
pub mod sinks;
pub mod state;

pub use event::{ConsoleEvent, Phase};
pub use render::{PromptChoice, PromptRequest};
pub use sinks::{ConsoleMode, JsonlSink, SimpleSink};
pub use state::{ActiveTask, BuildState, TaskState};

/// Cheap-to-clone handle to the build-event console.
///
/// Mirrors the [`LogManager`](super::context::LogManager) pattern: a
/// thin façade over an `Arc<Inner>` that can be threaded through the
/// scheduler, transforms, and CLI.
#[derive(Clone, Default)]
pub struct Console {
    inner: Arc<Inner>,
}

/// Inner state behind the [`Console`]'s `Arc`.
#[derive(Default)]
struct Inner {
    /// Sinks that receive every emitted event. `Arc<dyn Sink>` so
    /// [`Console::emit`] can clone the slice and release the mutex
    /// before fanning out — IO inside a sink must not serialise tokio
    /// workers (P1).
    sinks: Mutex<Vec<Arc<dyn Sink>>>,
    /// Optional handle to the inline canvas render task. Present when
    /// [`Console::install_canvas`] has been called.
    canvas: Mutex<Option<CanvasHandle>>,
}

/// Handle to the spawned canvas task.
struct CanvasHandle {
    tx: tokio::sync::mpsc::UnboundedSender<render::RenderMsg>,
    join: Option<tokio::task::JoinHandle<()>>,
}

/// Trait implemented by every console sink (JSONL, simple stderr, canvas).
pub trait Sink: Send + Sync {
    /// Receive one event. Implementations must be infallible — sinks
    /// that fail (e.g. JSONL file IO) log to `tracing::warn` and
    /// continue.
    fn handle(&self, event: &ConsoleEvent);
}

impl Console {
    /// Creates an empty [`Console`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a sink. Every subsequent [`emit`](Self::emit) call
    /// fans out to it.
    pub fn add_sink<S: Sink + 'static>(&self, sink: S) {
        self.inner.sinks.lock().push(Arc::new(sink));
    }

    /// Spawn the inline canvas render task and register a sink that
    /// forwards every [`ConsoleEvent`] to it. Idempotent: subsequent
    /// calls are a no-op.
    pub fn install_canvas(&self, height: u16) {
        let mut guard = self.inner.canvas.lock();
        if guard.is_some() {
            return;
        }
        let (tx, join) = render::spawn_render_task(height);
        let sink_tx = tx.clone();
        // Drop the lock before registering the sink so we don't hold
        // both locks.
        *guard = Some(CanvasHandle {
            tx,
            join: Some(join),
        });
        drop(guard);
        self.add_sink(CanvasSink { tx: sink_tx });
    }

    /// Publishes an event to every registered sink.
    ///
    /// The sinks mutex is released **before** dispatch so a slow sink
    /// (JSONL flush, canvas channel send) cannot block other workers
    /// from emitting (P1). We snapshot the `Arc<dyn Sink>` list under
    /// the lock and dispatch over the snapshot.
    pub fn emit(&self, event: ConsoleEvent) {
        let snapshot: Vec<Arc<dyn Sink>> = {
            let sinks = self.inner.sinks.lock();
            sinks.iter().cloned().collect()
        };
        for sink in &snapshot {
            sink.handle(&event);
        }
    }

    /// Drive an interactive failure prompt on the inline canvas. Blocks
    /// until the user picks `Retry` or `Quit`.
    ///
    /// When no canvas is installed (e.g. `--console-mode=simple|none`),
    /// the prompt cannot run on screen — the function returns
    /// [`PromptChoice::Quit`] immediately. Callers should detect this
    /// and surface a stderr explanation.
    pub async fn prompt(&self, request: PromptRequest) -> PromptChoice {
        let tx = match self.inner.canvas.lock().as_ref() {
            Some(h) => h.tx.clone(),
            None => return PromptChoice::Quit,
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        if tx
            .send(render::RenderMsg::Prompt {
                request,
                response: resp_tx,
            })
            .is_err()
        {
            return PromptChoice::Quit;
        }
        resp_rx.await.unwrap_or(PromptChoice::Quit)
    }

    /// Shut the canvas render task down cleanly. Idempotent.
    pub async fn shutdown(&self) {
        let handle = self.inner.canvas.lock().take();
        if let Some(mut h) = handle {
            let (ack_tx, ack_rx) = oneshot::channel();
            let _ = h.tx.send(render::RenderMsg::Shutdown { ack: ack_tx });
            let _ = ack_rx.await;
            if let Some(join) = h.join.take() {
                let _ = join.await;
            }
        }
    }

    /// Returns true when the inline canvas is active.
    pub fn has_canvas(&self) -> bool {
        self.inner.canvas.lock().is_some()
    }
}

/// Sink that forwards every [`ConsoleEvent`] into the canvas render
/// task. Cheap to clone — ratatui draws are bounded by the tick rate.
struct CanvasSink {
    tx: tokio::sync::mpsc::UnboundedSender<render::RenderMsg>,
}

impl Sink for CanvasSink {
    fn handle(&self, event: &ConsoleEvent) {
        // Channel send is non-blocking on UnboundedSender. Errors mean
        // the render task has exited (e.g. shutdown) — we silently
        // drop, since other sinks (JSONL, file log) still record.
        let _ = self.tx.send(render::RenderMsg::Event(event.clone()));
    }
}

// Console internal types live in `super` modules.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_default_is_silent() {
        let c = Console::new();
        // No sinks registered — emit must not panic.
        c.emit(ConsoleEvent::BuildFinished {
            ok: true,
            elapsed_ms: 0,
            failed: vec![],
        });
    }

    #[test]
    fn console_clone_shares_inner() {
        let a = Console::new();
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.inner, &b.inner));
    }

    #[test]
    fn prompt_without_canvas_returns_quit() {
        // `prompt` is async but we only need a smoke test that hits
        // the early-return branch. Use a basic runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let c = Console::new();
            let req = PromptRequest {
                addr: crate::context::Addr::parse("//x").unwrap(),
                error: "boom".into(),
                log_file: None,
                allow_retry: false,
                allow_shell: false,
                shell: None,
            };
            assert_eq!(c.prompt(req).await, PromptChoice::Quit);
            assert!(!c.has_canvas());
        });
    }
}
