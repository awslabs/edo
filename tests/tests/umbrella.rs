use edo_integration_tests::common::*;
use predicates::str::contains;
use serial_test::serial;

#[test]
fn cross_project_dep_resolves() {
    let fx = copy_umbrella();
    fx.edo(&["run", "//cross_project_consumer/final"]).success();
}

#[test]
fn list_from_umbrella_root() {
    let fx = copy_umbrella();
    fx.edo(&["list"])
        .success()
        .stdout(contains("//hello_local/emit"))
        .stdout(contains("//hello_script/build"))
        .stdout(contains("//hello_compose/bundle"))
        .stdout(contains("//cross_project_consumer/final"));
}

#[test]
#[serial]
fn manual_smoke() {
    // Regressions the link between the committed in-tree fixtures and the
    // manual-run cheatsheet in tests/fixtures/README.md. Edo writes
    // `edo.lock.json` into the CWD on load, so we cannot invoke it directly
    // against the repo — we copy first, and assert the same commands the
    // README advertises succeed against an identical tree.
    let fx = copy_umbrella();
    fx.edo(&["list"])
        .success()
        .stdout(contains("//hello_local/emit"));
    fx.edo(&["run", "//hello_compose/bundle"]).success();
    assert!(
        fx.path.join("README.md").is_file(),
        "manual README missing from umbrella copy",
    );
}
