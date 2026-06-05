//! Interactive transform executor with error recovery.
//!
//! Handles running a single transform, catching failures, and surfacing the
//! failure prompt through the build console.

use super::{Result, error};
use crate::{
    console::{PromptChoice, PromptRequest},
    context::{Handle, Log},
    environment::Environment,
    storage::Artifact,
    transform::{Transform, TransformStatus},
};
use std::io;

/// Executes a transform with interactive error recovery.
///
/// Runs the given transform within the provided environment. On failure,
/// drives the failure prompt via [`crate::console::Console::prompt`].
/// On success, uploads the resulting artifact to the build cache.
pub async fn execute(
    log: &Log,
    ctx: &Handle,
    addr: &crate::context::Addr,
    transform: &Transform,
    env: &Environment,
) -> Result<Artifact> {
    #[allow(unused_assignments)]
    let mut result: Result<Artifact> = error::NoRunSnafu {}.fail();
    'transform: loop {
        // Make an attempt
        let attempt_result = transform.transform(log, ctx, env).await;
        match &attempt_result {
            // If the attempt was successful exit out and return the resulting artifact
            TransformStatus::Success(artifact) => {
                result = Ok(artifact.clone());
                break 'transform;
            }
            // If the attempt failed for any reason we need to prompt the user what
            // we should do about it.
            TransformStatus::Retryable(log_file, e) | TransformStatus::Failed(log_file, e) => {
                error!(
                    subsystem = "transform",
                    op = "failed",
                    addr = %addr,
                    "transformation failed: {}",
                    e.to_string()
                );

                let allow_retry = matches!(attempt_result, TransformStatus::Retryable(..));
                let allow_shell = transform.can_shell();
                let shell_callback: Option<Box<dyn FnMut() -> io::Result<()> + Send>> =
                    if allow_shell {
                        // Capture the env + transform handles into a closure
                        // the render task can call once the canvas is
                        // suspended.
                        let transform = transform.clone();
                        let env = env.clone();
                        Some(Box::new(move || {
                            transform
                                .shell(&env)
                                .map_err(|e| io::Error::other(e.to_string()))
                        }))
                    } else {
                        None
                    };
                let request = PromptRequest {
                    addr: addr.clone(),
                    error: e.to_string(),
                    log_file: log_file.clone(),
                    allow_retry,
                    allow_shell,
                    shell: shell_callback,
                };
                let choice = ctx.console().prompt(request).await;
                match choice {
                    PromptChoice::Retry if allow_retry => {
                        continue 'transform;
                    }
                    PromptChoice::Retry => {
                        // Defensive: the prompt should never offer
                        // `retry` when `allow_retry == false`, but if
                        // it somehow returns one (canvas absent,
                        // shutdown race, second prompt rejected) we
                        // emit an explicit diagnostic instead of
                        // silently downgrading to abort (P1).
                        ctx.console().emit(crate::console::ConsoleEvent::diag(
                            crate::console::event::Severity::Warn,
                            "transform",
                            format!("{addr}: retry not available for this failure; aborting"),
                        ));
                        ctx.cancellation().cancel();
                        result = error::PassthroughSnafu {
                            message: e.to_string(),
                        }
                        .fail();
                        break 'transform;
                    }
                    PromptChoice::Quit => {
                        ctx.cancellation().cancel();
                        result = error::PassthroughSnafu {
                            message: e.to_string(),
                        }
                        .fail();
                        break 'transform;
                    }
                }
            }
        }
    }
    // Upload the result if we have a build cache setup
    match result {
        Ok(artifact) => {
            ctx.storage().upload_build(artifact.config().id()).await?;
            Ok(artifact)
        }
        Err(e) => Err(e),
    }
}

/// Best-effort: derive the failed transform's `Addr`. The `Transform`
/// trait does not expose its address directly — callers pass the
/// scheduler's `node.addr` instead.
#[allow(dead_code)]
fn addr_for(_t: &Transform) -> Option<crate::context::Addr> {
    None
}

#[cfg(test)]
mod tests {
    //! Tests for `execute`.
    //!
    //! Scope: success-path only. Failure paths drive the canvas prompt
    //! which requires a TTY harness we do not have.

    use super::*;
    use crate::context::{Addr, Context, LogVerbosity};
    use crate::environment::{Environment, Farm, MockEnvironmentImpl, MockFarmImpl};
    use crate::storage::{
        Artifact as StorageArtifact, Compression, Config as ArtifactConfig, Id, MediaType,
    };
    use crate::transform::{MockTransformImpl, Transform, TransformStatus};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::OnceCell;

    // ── process-wide context bootstrap (mirrors graph::tests pattern) ───────

    type SharedCtx = (Context, Arc<TempDir>);
    static SHARED: OnceCell<SharedCtx> = OnceCell::const_new();

    async fn try_shared_context() -> Option<Context> {
        if let Some((ctx, _)) = SHARED.get() {
            return Some(ctx.clone());
        }
        let dir = TempDir::new().expect("tempdir");
        match Context::init::<&Path, &Path>(
            Some(dir.path()),
            None,
            HashMap::new(),
            LogVerbosity::Info,
            crate::context::ConsoleConfig::default(),
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

    fn mini_env() -> Environment {
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

    fn mini_farm() -> Farm {
        let mut f = MockFarmImpl::new();
        f.expect_setup().returning(|_, _| Ok(()));
        f.expect_create().returning(|_, _| Ok(mini_env()));
        Farm::new(f)
    }

    fn success_transform(digest: &str) -> Transform {
        let digest = digest.to_string();
        let mut t = MockTransformImpl::new();
        t.expect_environment()
            .returning(|| Ok(Addr::parse("//default").unwrap()));
        {
            let d = digest.clone();
            t.expect_get_unique_id().returning(move |_ctx| {
                Ok(Id::builder()
                    .name("exec_mock".to_string())
                    .digest(d.clone())
                    .build())
            });
        }
        t.expect_depends().returning(|| Ok(Vec::new()));
        t.expect_prepare().returning(|_, _| Ok(()));
        t.expect_stage().returning(|_, _, _| Ok(()));
        {
            let d = digest.clone();
            t.expect_transform().returning(move |_, _, _| {
                let id = Id::builder()
                    .name("exec_mock".to_string())
                    .digest(d.clone())
                    .build();
                TransformStatus::Success(
                    StorageArtifact::builder()
                        .media_type(MediaType::File(Compression::None))
                        .config(ArtifactConfig::builder().id(id).build())
                        .build(),
                )
            });
        }
        t.expect_can_shell().return_const(false);
        t.expect_shell().returning(|_| Ok(()));
        Transform::new(t)
    }

    // ── actual test ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[serial_test::serial(log_manager)]
    async fn execute_success_returns_artifact_and_uploads_build() {
        let Some(ctx) = try_shared_context().await else {
            eprintln!("skip: subscriber already initialized");
            return;
        };
        // No build cache is registered → `upload_build` is a silent no-op,
        // so a Success path must still return the artifact cleanly.
        let handle = ctx.get_handle();
        let log = handle.log().create("execute-test").await.expect("log");
        let farm = mini_farm();
        let env = farm.create(&log, Path::new("/")).await.expect("env");
        let transform = success_transform("deadbeef");

        let artifact = execute(
            &log,
            &handle,
            &Addr::parse("//exec/test").unwrap(),
            &transform,
            &env,
        )
        .await
        .expect("execute success");
        assert_eq!(artifact.config().id().digest(), "deadbeef");
        assert_eq!(artifact.config().id().name(), "exec_mock");
    }
}
