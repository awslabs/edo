//! Golden-shape tests for the structured event stream written by the
//! Phase 2 [`JsonlSink`].
//!
//! These tests run a real `edo run` against a fixture but, unlike the
//! rest of the integration suite, leave `--event-log` at its default so
//! that the JSONL file is produced. They assert that:
//!
//! 1. the file is created at `<storage>/events.jsonl`,
//! 2. every line is valid JSON,
//! 3. the canonical event sequence is present in order
//!    (`session-started` → `project-loaded` → `env-setup-started` →
//!    `env-setup-finished` → `build-started` → at least one node
//!    lifecycle → `build-finished`),
//! 4. `build-finished.ok = true` and `failed = []` for a green build.
//!
//! We do *not* assert exact content beyond shape — phase counts and
//! cache-hit interleaving depend on environmental state that the
//! `--storage` isolation already minimizes but does not eliminate.

use std::path::PathBuf;

use assert_cmd::Command;
use edo_integration_tests::common::*;
use serde_json::Value;
use tempfile::TempDir;

/// Like `Fixture::edo` but does NOT inject `--event-log=none`, so the
/// JSONL sink writes to `<storage>/events.jsonl` (its default path).
/// `--console-mode=simple` keeps stderr deterministic-ish without
/// tripping the (Phase 2) auto-detect path.
fn run_with_jsonl(fx: &Fixture, args: &[&str]) {
    let mut c = Command::cargo_bin("edo-cli").expect("edo-cli binary");
    c.current_dir(&fx.path).env_remove("RUST_LOG");
    c.arg("--storage").arg(&fx.storage);
    c.arg("--console-mode=simple");
    for a in args {
        c.arg(a);
    }
    c.assert().success();
}

fn event_log_path(fx: &Fixture) -> PathBuf {
    fx.storage.join("events.jsonl")
}

fn read_events(path: &PathBuf) -> Vec<Value> {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("event line is valid JSON"))
        .collect()
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["type"].as_str().expect("every event has a string `type`"))
        .collect()
}

#[test]
fn jsonl_sink_writes_canonical_event_sequence() {
    let fx = copy_fixture("hello_local");
    run_with_jsonl(&fx, &["run", "//hello_local/emit"]);

    let path = event_log_path(&fx);
    assert!(
        path.exists(),
        "events.jsonl should exist at {}",
        path.display()
    );

    let events = read_events(&path);
    assert!(!events.is_empty(), "events.jsonl should not be empty");

    let kinds = types(&events);

    // Provenance prologue: session-started → project-loaded come first
    // in that exact order. The env-setup phase and build-started
    // follow, in that order, but other events (e.g. zero-farm fixtures
    // would skip the per-farm pair) may sit between. We assert
    // ordering rather than fixed positions.
    assert_eq!(
        kinds.first().copied(),
        Some("session-started"),
        "kinds: {kinds:?}"
    );
    assert_eq!(
        kinds.get(1).copied(),
        Some("project-loaded"),
        "kinds: {kinds:?}"
    );
    assert_eq!(
        kinds.last().copied(),
        Some("build-finished"),
        "kinds: {kinds:?}"
    );
    let pos = |name: &str| kinds.iter().position(|k| *k == name);
    let env_start = pos("env-setup-started").expect("env-setup-started present");
    let env_end = pos("env-setup-finished").expect("env-setup-finished present");
    let build_start = pos("build-started").expect("build-started present");
    assert!(
        env_start < env_end,
        "env-setup-started before env-setup-finished"
    );
    assert!(
        env_end < build_start,
        "env-setup-finished before build-started"
    );
    assert!(env_start > 1, "env-setup-started after the prologue");

    // session-started carries provenance: edo_version (string), target
    // (string matching what we asked for), started_at_unix (number).
    let session = &events[0];
    assert!(
        session["edo_version"].is_string(),
        "session-started: {session}"
    );
    assert_eq!(
        session["target"],
        Value::String("//hello_local/emit".into())
    );
    assert!(
        session["started_at_unix"].is_number(),
        "session-started: {session}"
    );

    // project-loaded carries counts and the locked flag.
    let project = &events[1];
    assert!(project["root"].is_string(), "project-loaded: {project}");
    assert!(
        project["transforms"].is_number(),
        "project-loaded: {project}"
    );
    assert!(project["locked"].is_boolean(), "project-loaded: {project}");

    // env-setup-started carries `total` (number of farms).
    let env_started = &events[env_start];
    assert!(
        env_started["total"].is_number(),
        "env-setup-started: {env_started}"
    );
    // env-setup-finished carries `elapsed_ms`.
    let env_finished = &events[env_end];
    assert!(
        env_finished["elapsed_ms"].is_number(),
        "env-setup-finished: {env_finished}"
    );

    // build-started carries a numeric `total` and string `root`.
    let started = &events[build_start];
    assert!(started["total"].is_number(), "build-started: {started}");
    assert!(started["root"].is_string(), "build-started: {started}");

    // We expect at least one node-level event in between (queued or cache-hit).
    let saw_node_activity = kinds.iter().any(|k| {
        matches!(
            *k,
            "node-queued" | "node-cache-hit" | "node-phase" | "node-finished"
        )
    });
    assert!(
        saw_node_activity,
        "expected node activity, kinds: {kinds:?}"
    );

    // Final summary is green.
    let finished = events.last().unwrap();
    assert_eq!(finished["ok"], Value::Bool(true), "finished: {finished}");
    assert_eq!(
        finished["failed"],
        Value::Array(vec![]),
        "finished: {finished}"
    );
    assert!(finished["elapsed_ms"].is_number(), "finished: {finished}");
}

#[test]
fn jsonl_node_phase_uses_kebab_case_phase() {
    // Use a fixture that actually executes a transform (script) so we
    // get NodePhase events on the run path, not just a cache hit.
    let fx = copy_fixture("hello_script");
    run_with_jsonl(&fx, &["run", "//hello_script/build"]);

    let events = read_events(&event_log_path(&fx));
    let phases: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "node-phase")
        .filter_map(|e| e["phase"].as_str())
        .collect();

    // If the build was fully cached we won't have node-phase events at
    // all — that's fine, this test is about *shape* when they appear.
    for p in &phases {
        // serde rename_all = "kebab-case" → known set:
        let known = matches!(
            *p,
            "fetch"
                | "wait"
                | "create-env"
                | "setup"
                | "spin-up"
                | "stage"
                | "execute"
                | "spin-down"
                | "clean"
        );
        assert!(known, "unknown phase {p:?} in {phases:?}");
    }
}

#[test]
fn event_log_none_disables_file() {
    // Sanity: --event-log=none (used by the rest of the suite) really
    // does prevent the file from being created.
    let fx = copy_fixture("hello_local");
    fx.edo(&["run", "//hello_local/emit"]).success();
    assert!(
        !event_log_path(&fx).exists(),
        "events.jsonl should NOT exist when --event-log=none"
    );
}

#[test]
fn explicit_event_log_path_is_honored() {
    let fx = copy_fixture("hello_local");
    let custom_dir = TempDir::new().expect("custom log dir");
    let custom = custom_dir.path().join("custom-events.jsonl");

    let mut c = Command::cargo_bin("edo-cli").expect("edo-cli binary");
    c.current_dir(&fx.path).env_remove("RUST_LOG");
    c.arg("--storage").arg(&fx.storage);
    c.arg("--console-mode=none");
    c.arg("--event-log").arg(&custom);
    c.arg("run").arg("//hello_local/emit");
    c.assert().success();

    assert!(
        custom.exists(),
        "custom event log {} not created",
        custom.display()
    );
    assert!(
        !event_log_path(&fx).exists(),
        "default events.jsonl should NOT be created when an explicit path is given"
    );

    // And the custom file is well-formed.
    let events = read_events(&custom);
    assert!(!events.is_empty());
    let kinds = types(&events);
    assert_eq!(kinds.first().copied(), Some("session-started"));
    assert_eq!(kinds.last().copied(), Some("build-finished"));
}
