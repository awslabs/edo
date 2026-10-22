//! `Context::init` installs a process-wide tracing dispatcher, so this test
//! lives in its own binary rather than alongside `run_ids.rs`.

use std::collections::HashMap;

use edo::context::{Addr, Context, Element, EventLog, IdCache, LogVerbosity};
use edo::source::SourceError;
use edo_core::register_core;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn local_source_edited_between_hash_and_fetch_is_rejected() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("input.txt");
    tokio::fs::write(&input, "original").await.unwrap();
    let ctx = Context::init(
        Some(dir.path().join("state")),
        Some(dir.path().join("config.toml")),
        HashMap::new(),
        LogVerbosity::Info,
        EventLog::Disabled,
    )
    .await
    .unwrap();
    register_core(&ctx);
    let source = ctx
        .add_source(
            &Element::builder()
                .addr(Addr::parse("//ids/edited").unwrap())
                .kind("local")
                .config([("path".into(), json!(input))])
                .build(),
        )
        .await
        .unwrap();
    let handle = ctx.get_handle().with_id_cache(IdCache::new());
    let memoized = source.cached_unique_id(&handle).await.unwrap();
    tokio::fs::write(&input, "changed").await.unwrap();

    let log = ctx.log().create("edited-source").await.unwrap();
    match source.cache_in(&log, &handle).await {
        Err(SourceError::Changed { expected, actual }) => {
            assert_eq!(*expected, memoized);
            assert_ne!(*actual, memoized);
        }
        other => panic!("expected SourceError::Changed, got {other:?}"),
    }
    assert!(!ctx.storage().has_local(&memoized).await.unwrap());
}
