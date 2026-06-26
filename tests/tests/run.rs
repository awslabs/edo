use edo_integration_tests::common::*;

#[test]
fn run_import_happy_path() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["run", "//hello_local/emit"]).success();
}

#[test]
fn run_script_happy_path() {
    let fx = copy_fixture("hello_script");
    fx.edo(&["run", "//hello_script/build"]).success();
}

#[test]
fn run_compose_merges_layers() {
    let fx = copy_fixture("hello_compose");
    fx.edo(&["run", "//hello_compose/bundle"]).success();
}

#[test]
fn run_bad_cwd_has_no_edo_toml() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = dir.path().join(".edo-test-store");
    let mut cmd = assert_cmd::Command::cargo_bin("edo-cli").unwrap();
    cmd.current_dir(dir.path())
        .env_remove("RUST_LOG")
        .arg("--storage")
        .arg(&storage)
        .arg("--console-mode=none")
        .arg("--event-log=none")
        .arg("run")
        .arg("//anything")
        .assert()
        .failure();
}

/// Regression: changing `out` on a `kind = "local"` source must invalidate
/// the cached manifest and re-stage with the new path. We assert by
/// reading the catalog and confirming both `path_hints` values
/// (`.` then `sub`) survived as distinct manifests.
#[test]
fn run_local_source_out_change_invalidates_manifest() {
    let fx = copy_fixture("hello_local");
    let toml_path = fx.path.join("hello_local").join("edo.toml");

    // First run with the committed `out = "."`.
    fx.edo(&["run", "//hello_local/emit"]).success();
    let first = read_path_hints_by_prefix(&fx.storage, "files");
    assert_eq!(first.len(), 1, "first run produces one source manifest");
    let first_hints: Vec<String> = first[0].values().cloned().collect();
    assert_eq!(first_hints, vec![".".to_string()]);

    // Edit `out` and re-run; this MUST produce a new manifest.
    replace_toml_string_value(&toml_path, "out", ".", "sub");

    fx.edo(&["run", "//hello_local/emit"]).success();
    let second = read_path_hints_by_prefix(&fx.storage, "files");
    assert_eq!(
        second.len(),
        2,
        "second run with new `out` produces a second manifest, got {second:?}"
    );
    let mut all_hints: Vec<String> = second.iter().flat_map(|m| m.values().cloned()).collect();
    all_hints.sort();
    assert_eq!(all_hints, vec![".".to_string(), "sub".to_string()]);
}
