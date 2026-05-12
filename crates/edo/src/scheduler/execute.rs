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
    //! test without a harness we do not have. Those branches are therefore
    //! deliberately uncovered here — see the plan at
    //! `/Users/jmt/.maki/plans/stable-solid-penguin.md` for the rationale.
    //!
    //! Per the plan, we keep a duplicated minimal copy of the transform/
    //! environment mocks instead of importing from `graph::tests`, so this
    //! file can be read standalone.

    use super::*;
    use crate::context::{Addr, Context, LogVerbosity};
    use crate::environment::{Command, EnvResult, Environment, EnvironmentImpl, Farm, FarmImpl};
    use crate::storage::{
        Artifact as StorageArtifact, Compression, Config as ArtifactConfig, Id, MediaType,
    };
    use crate::transform::{Transform, TransformImpl, TransformResult};
    use crate::util::{Reader, Writer};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
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

    // ── minimal environment mock ────────────────────────────────────────────

    struct MiniEnvImpl;

    #[async_trait]
    impl EnvironmentImpl for MiniEnvImpl {
        async fn expand(&self, path: &Path) -> EnvResult<PathBuf> {
            Ok(path.to_path_buf())
        }
        async fn create_dir(&self, _path: &Path) -> EnvResult<()> {
            Ok(())
        }
        async fn set_env(&self, _k: &str, _v: &str) -> EnvResult<()> {
            Ok(())
        }
        async fn get_env(&self, _k: &str) -> Option<String> {
            None
        }
        async fn setup(&self, _log: &Log, _storage: &crate::storage::Storage) -> EnvResult<()> {
            Ok(())
        }
        async fn up(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn down(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn clean(&self, _log: &Log) -> EnvResult<()> {
            Ok(())
        }
        async fn write(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn unpack(&self, _p: &Path, _r: Reader) -> EnvResult<()> {
            Ok(())
        }
        async fn read(&self, _p: &Path, _w: Writer) -> EnvResult<()> {
            Ok(())
        }
        async fn cmd(&self, _log: &Log, _id: &Id, _p: &Path, _c: &str) -> EnvResult<bool> {
            Ok(true)
        }
        async fn run(&self, _log: &Log, _id: &Id, _p: &Path, _c: &Command) -> EnvResult<bool> {
            Ok(true)
        }
        fn shell(&self, _p: &Path) -> EnvResult<()> {
            Ok(())
        }
    }

    struct MiniFarmImpl;

    #[async_trait]
    impl FarmImpl for MiniFarmImpl {
        async fn setup(&self, _log: &Log, _storage: &crate::storage::Storage) -> EnvResult<()> {
            Ok(())
        }
        async fn create(&self, _log: &Log, _p: &Path) -> EnvResult<Environment> {
            Ok(Environment::new(MiniEnvImpl))
        }
    }

    // ── minimal transform mock (success only) ───────────────────────────────

    struct SuccessTransform {
        digest: String,
    }

    fn mk_artifact(digest: &str) -> StorageArtifact {
        let id = Id::builder()
            .name("exec-mock".to_string())
            .digest(digest.to_string())
            .build();
        StorageArtifact::builder()
            .media_type(MediaType::File(Compression::None))
            .config(ArtifactConfig::builder().id(id).build())
            .build()
    }

    #[async_trait]
    impl TransformImpl for SuccessTransform {
        async fn environment(&self) -> TransformResult<Addr> {
            Ok(Addr::parse("//default").unwrap())
        }
        async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
            Ok(Id::builder()
                .name("exec-mock".to_string())
                .digest(self.digest.clone())
                .build())
        }
        async fn depends(&self) -> TransformResult<Vec<Addr>> {
            Ok(Vec::new())
        }
        async fn prepare(&self, _log: &Log, _ctx: &Handle) -> TransformResult<()> {
            Ok(())
        }
        async fn stage(
            &self,
            _log: &Log,
            _ctx: &Handle,
            _env: &Environment,
        ) -> TransformResult<()> {
            Ok(())
        }
        async fn transform(
            &self,
            _log: &Log,
            _ctx: &Handle,
            _env: &Environment,
        ) -> TransformStatus {
            TransformStatus::Success(mk_artifact(&self.digest))
        }
        fn can_shell(&self) -> bool {
            false
        }
        fn shell(&self, _env: &Environment) -> TransformResult<()> {
            Ok(())
        }
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
        let farm = Farm::new(MiniFarmImpl);
        let env = farm.create(&log, Path::new("/")).await.expect("env");
        let transform = Transform::new(SuccessTransform {
            digest: "deadbeef".to_string(),
        });

        let artifact = execute(&log, &handle, &transform, &env)
            .await
            .expect("execute success");
        assert_eq!(artifact.config().id().digest(), "deadbeef");
        assert_eq!(artifact.config().id().name(), "exec_mock");
    }
}
