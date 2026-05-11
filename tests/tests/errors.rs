use edo_integration_tests::common::*;

#[test]
fn bad_address_nonzero_exit() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["run", "//not-an-addr"]).failure();
}

#[test]
fn bad_toml_nonzero_exit() {
    let fx = copy_from(&error_fixtures_root(), "bad_toml");
    fx.edo(&["list"]).failure();
}

#[test]
fn unresolved_source_nonzero_exit() {
    let fx = copy_from(&error_fixtures_root(), "unresolved_source");
    fx.edo(&["list"]).failure();
}
