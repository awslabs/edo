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

#[test]
fn invalid_schema_is_rejected_during_loading() {
    for input in [
        "[source.src]\nkind = \"local\"\npath = \"x\"",
        "schema-version = \"99\"",
        "schema-version = \"1\"\n[transfrom.build]\nkind = \"script\"",
        "schema-version = \"1\"\n[cache.buid]\nkind = \"local\"",
        "schema-version = \"1\"\n[requires.gcc]\nkind = \"image\"\nat = \"*\"\nvendro = \"//v\"",
    ] {
        let fx = copy_fixture("hello_local");
        std::fs::write(fx.path.join("hello_local/edo.toml"), input).unwrap();
        fx.edo(&["list"])
            .failure()
            .stderr(predicates::str::contains("failed to deserialize toml"));
    }
}
