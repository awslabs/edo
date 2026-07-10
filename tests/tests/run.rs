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
fn script_reads_staged_files_from_working_directory() {
    let fx = copy_fixture("hello_script");
    let path = fx.path.join("hello_script/edo.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        original.replace("sh {{build-root}}/make_hello.sh", "sh make_hello.sh"),
    )
    .unwrap();
    fx.edo(&["run", "//hello_script/build"]).success();
    let out = fx.dir.path().join("out");
    fx.edo(&["checkout", "//hello_script/build", out.to_str().unwrap()])
        .success();
    assert!(
        std::fs::read_to_string(out.join("hello.txt"))
            .unwrap()
            .contains("script-produced hello")
    );
}

#[test]
fn relative_environment_and_dependencies_resolve_in_nested_project() {
    let fx = copy_fixture("hello_script");
    std::fs::write(
        fx.path.join("hello_script/edo.toml"),
        r#"schema-version = "1"
[environment.local]
kind = "local"
[transform.first]
kind = "script"
environment = "local"
commands = ["printf staged > {{install-root}}/input.txt"]
[transform."group/second"]
kind = "script"
environment = "local"
depends = ["first"]
commands = ["cp input.txt {{install-root}}/result.txt"]
[transform.bundle]
kind = "compose"
depends = ["group/second", "//hello_script/first"]
"#,
    )
    .unwrap();
    std::fs::write(
        fx.path.join("edo.toml"),
        r#"schema-version = "1"
[environment.local]
kind = "local"
[transform.first]
kind = "script"
commands = ["printf wrong > {{install-root}}/input.txt"]
"#,
    )
    .unwrap();
    fx.edo(&["run", "//hello_script/bundle"]).success();
    let out = fx.dir.path().join("out");
    fx.edo(&["checkout", "//hello_script/bundle", out.to_str().unwrap()])
        .success();
    assert_eq!(
        std::fs::read_to_string(out.join("result.txt")).unwrap(),
        "staged"
    );
}

#[test]
fn cli_architecture_overrides_script_and_compose() {
    let fx = copy_fixture("hello_script");
    std::fs::write(
        fx.path.join("hello_script/edo.toml"),
        r#"schema-version = "1"
[transform.build]
kind = "script"
arch = "x86_64"
commands = ["printf '{{arch}}' > {{install-root}}/arch.txt"]
[transform.bundle]
kind = "compose"
arch = "x86_64"
depends = ["build"]
"#,
    )
    .unwrap();
    for (arch, args) in [
        ("aarch64", vec!["--arg", "arch=aarch64"]),
        ("x86_64", vec![]),
    ] {
        let mut run = vec!["run", "//hello_script/bundle"];
        run.extend(&args);
        fx.edo(&run).success();
        let out = fx.dir.path().join(arch);
        let mut checkout = vec!["checkout", "//hello_script/bundle", out.to_str().unwrap()];
        checkout.extend(&args);
        fx.edo(&checkout).success();
        assert_eq!(std::fs::read_to_string(out.join("arch.txt")).unwrap(), arch);
    }
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fx.storage.join("storage/catalog.json")).unwrap())
            .unwrap();
    let manifests = catalog["manifests"].as_object().unwrap();
    for name in ["hello_script_build", "hello_script_bundle"] {
        for arch in ["aarch64", "x86_64"] {
            assert_eq!(
                manifests
                    .keys()
                    .filter(|id| id.starts_with(&format!("{name}.{arch}:")))
                    .count(),
                1
            );
        }
    }
    for (id, manifest) in manifests
        .iter()
        .filter(|(id, _)| id.starts_with("hello_script_build."))
    {
        let arch = id
            .strip_prefix("hello_script_build.")
            .unwrap()
            .split(':')
            .next()
            .unwrap();
        assert_eq!(manifest["layers"][0]["platform"]["architecture"], arch);
    }
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
