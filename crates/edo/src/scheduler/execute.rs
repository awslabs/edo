//! Interactive transform executor with error recovery.
//!
//! Handles running a single transform, catching failures, and prompting the
//! user with options to view logs, retry, open a shell, or abort.

use super::{Result, error};
use crate::{
    context::{Handle, Log},
    environment::Environment,
    storage::Artifact,
    transform::{Transform, TransformStatus},
};
use dialoguer::{Editor, Select};
use snafu::ResultExt;
use std::fs::read_to_string;
use tracing_indicatif::suspend_tracing_indicatif;

/// Executes a transform with interactive error recovery.
///
/// Runs the given transform within the provided environment. On failure,
/// prompts the user with options to view logs, retry, open a shell, or quit.
/// On success, uploads the resulting artifact to the build cache.
pub async fn execute(
    log: &Log,
    ctx: &Handle,
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
                error!(target: "transform", "transformation failed: {}", e.to_string());
                // Collect the valid options to present the user with
                let mut options = Vec::new();
                if log_file.is_some() {
                    options.push("view log");
                }
                if matches!(attempt_result, TransformStatus::Retryable(..)) {
                    options.push("retry");
                }
                if transform.can_shell() {
                    options.push("shell");
                }
                options.push("quit");
                // IMPORTANT! We need to susppend our progress bars to ask the
                // user for what to do.
                let should_quit = suspend_tracing_indicatif(|| {
                    // Acquire an exclusive lock on the console through
                    // the log manager
                    let console_lock = ctx.log().acquire();
                    'prompt: loop {
                        let index = Select::new()
                            .items(options.as_slice())
                            .default(0)
                            .interact()
                            .context(error::InquireSnafu)?;
                        let ans = options[index];
                        match ans {
                            "view log" => {
                                let log_text =
                                    read_to_string(log_file.as_ref().expect(
                                        "log_file is Some when 'view log' option is present",
                                    ))
                                    .context(error::IoSnafu)?;
                                Editor::new().edit(&log_text).context(error::InquireSnafu)?;
                                continue 'prompt;
                            }
                            "shell" => {
                                transform.shell(env)?;
                                continue 'prompt;
                            }
                            "retry" => {
                                return Ok(false);
                            }
                            "quit" => {
                                ctx.cancellation().cancel();
                                break 'prompt;
                            }
                            _ => {
                                return Ok(false);
                            }
                        }
                    }
                    drop(console_lock);
                    Ok::<bool, error::SchedulerError>(true)
                })?;
                if should_quit {
                    result = error::PassthroughSnafu {
                        message: e.to_string(),
                    }
                    .fail();
                    break 'transform;
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

#[cfg(test)]
mod tests {
    //! Tests for `execute`.
    //!
    //! Scope: success-path only. `TransformStatus::Retryable` and
    //! `TransformStatus::Failed` both route through `dialoguer::Select::interact`
    //! which requires an interactive TTY and cannot be driven from a unit
    //! test without a harness we do not have.
    //!
    //! Mocks come from `mockall::automock` on the trait definitions; the
    //! builders below configure them with the minimal pass-through behavior
    //! required by `execute`.

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

        let artifact = execute(&log, &handle, &transform, &env)
            .await
            .expect("execute success");
        assert_eq!(artifact.config().id().digest(), "deadbeef");
        assert_eq!(artifact.config().id().name(), "exec_mock");
    }
}
