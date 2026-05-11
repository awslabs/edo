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
        .arg("run")
        .arg("//anything")
        .assert()
        .failure();
}
