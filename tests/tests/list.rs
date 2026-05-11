use edo_integration_tests::common::*;
use predicates::str::contains;

#[test]
fn list_includes_hello_local() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["list"])
        .success()
        .stdout(contains("//hello_local/emit"));
}

#[test]
fn list_umbrella_shows_all_offline() {
    let fx = copy_umbrella();
    let assertion = fx.edo(&["list"]).success();
    assertion
        .stdout(contains("//hello_local/emit"))
        .stdout(contains("//hello_script/build"))
        .stdout(contains("//hello_compose/bundle"))
        .stdout(contains("//hello_compose/left"))
        .stdout(contains("//hello_compose/right"))
        .stdout(contains("//cross_project_consumer/final"));
}
