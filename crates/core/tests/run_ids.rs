use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use edo::context::{Addr, Context, Element, EventLog, LogVerbosity, SourceMap};
use edo::scheduler::graph::Graph;
use edo::storage::ArtifactStageOptions;
use edo_core::register_core;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn local_source_changes_after_fetch_preserve_staged_and_output_ids() {
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
    ctx.add_farm(
        &Element::builder()
            .addr(Addr::parse("//default").unwrap())
            .kind("local")
            .config(BTreeMap::new())
            .build(),
    )
    .await
    .unwrap();
    let source = Element::builder()
        .addr(Addr::parse("//ids/source").unwrap())
        .kind("local")
        .config([("path".into(), json!(input))])
        .build();
    let import = Addr::parse("//ids/import").unwrap();
    let script = Addr::parse("//ids/script").unwrap();
    let compose = Addr::parse("//ids/compose").unwrap();
    ctx.add_transform(
        &Element::builder()
            .addr(import.clone())
            .kind("import")
            .source(SourceMap::Resolved(BTreeMap::from([(
                "default".into(),
                vec![source],
            )])))
            .config(BTreeMap::new())
            .build(),
    )
    .await
    .unwrap();
    ctx.add_transform(
        &Element::builder()
            .addr(script.clone())
            .kind("script")
            .config([
                ("depends".into(), json!([import])),
                (
                    "commands".into(),
                    json!(["cp {{build-root}}/input.txt {{install-root}}/output.txt"]),
                ),
            ])
            .build(),
    )
    .await
    .unwrap();
    ctx.add_transform(
        &Element::builder()
            .addr(compose.clone())
            .kind("compose")
            .config([("depends".into(), json!([script]))])
            .build(),
    )
    .await
    .unwrap();

    let mut graph = Graph::new(2);
    graph.add(&ctx, &compose).await.unwrap();
    let handle = ctx.get_handle();
    let mut expected = Vec::new();
    for addr in [&import, &script, &compose] {
        expected.push(handle.unique_id_of(addr).await.unwrap());
    }
    graph.fetch(&ctx).await.unwrap();
    tokio::fs::write(&input, "changed").await.unwrap();
    assert_ne!(handle.unique_id_of(&import).await.unwrap(), expected[0]);
    let workspace = dir.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        graph.run(Path::new(&workspace), &ctx, &compose),
    )
    .await
    .unwrap()
    .unwrap();
    for id in &expected {
        assert!(ctx.storage().has_local(id).await.unwrap());
    }
    let log = ctx.log().create("verify-output").await.unwrap();
    let env = handle
        .create_environment(
            &log,
            &Addr::parse("//default").unwrap(),
            &dir.path().join("checkout"),
        )
        .await
        .unwrap();
    env.create_dir(Path::new("output")).await.unwrap();
    env.stage(
        &handle,
        ArtifactStageOptions::builder()
            .id(expected.last().unwrap().clone())
            .path(Path::new("output"))
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(
        env.read_bytes(Path::new("output/output.txt"))
            .await
            .unwrap(),
        b"original"
    );
}
