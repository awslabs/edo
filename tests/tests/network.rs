use edo_integration_tests::common::*;

#[test]
fn run_git_source() {
    let fx = copy_from(&net_fixtures_root(), "net_git");
    fx.edo(&["run", "//net_git/build"]).success();
}

#[test]
fn run_remote_source() {
    let fx = copy_from(&net_fixtures_root(), "net_remote");
    fx.edo(&["run", "//net_remote/build"]).success();
}

#[test]
fn run_script_in_container() {
    if !container_enabled() {
        eprintln!("skip: EDO_TEST_CONTAINER not set or no podman/docker on PATH");
        return;
    }
    let fx = copy_from(&net_fixtures_root(), "net_container_script");
    fx.edo(&["run", "//net_container_script/build"]).success();
}

#[test]
fn run_cargo_vendor_build() {
    let fx = copy_from(&net_fixtures_root(), "rust_src");
    fx.edo(&["run", "//rust_src/build"]).success();
}

#[test]
fn run_go_vendor_build() {
    let fx = copy_from(&net_fixtures_root(), "go_src");
    fx.edo(&["run", "//go_src/build"]).success();
}

/// Regression: changing `out` on a `kind = "remote"` source must
/// invalidate the cached manifest **and** reuse the existing blob (the
/// content-digest reference did not change, only the staging path did).
///
/// Asserts:
/// * two manifests carrying different `path_hints` values exist after run 2;
/// * the blob count for the remote `ref` digest is exactly 1.
#[test]
fn run_remote_source_out_change_reuses_blob() {
    let fx = copy_from(&net_fixtures_root(), "net_remote");
    let toml_path = fx.path.join("net_remote").join("edo.toml");

    fx.edo(&["run", "//net_remote/build"]).success();
    let blobs_after_first = count_blobs(&fx.storage);

    replace_toml_string_value(&toml_path, "out", "README", "docs/README");

    fx.edo(&["run", "//net_remote/build"]).success();

    // Two distinct source manifests, one per `out` value.
    let manifests = read_path_hints_by_prefix(&fx.storage, "octocat_Hello_World_master_README");
    assert!(
        manifests.len() >= 2,
        "expected at least two `octocat_Hello_World_master_README` manifests after `out` \
         change, got {manifests:?}"
    );
    let mut all_hints: Vec<String> = manifests.iter().flat_map(|m| m.values().cloned()).collect();
    all_hints.sort();
    assert!(
        all_hints.contains(&"README".to_string()) && all_hints.contains(&"docs/README".to_string()),
        "both `out` values must appear in path_hints, got {all_hints:?}"
    );

    // The remote source's blob is content-addressed by `ref`; the
    // `out` change must NOT have introduced a second copy. The
    // import transform writes its own output blob, so total blob
    // count for the second run should be `first + 1` (the new
    // `import` output), NOT `first + 2`.
    let blobs_after_second = count_blobs(&fx.storage);
    assert_eq!(
        blobs_after_second,
        blobs_after_first + 1,
        "remote blob must be reused on `out` change \
         (first={blobs_after_first}, second={blobs_after_second})"
    );
}

/// Regression: changing `out` on a `kind = "git"` source must invalidate
/// the cached manifest. Git sources are not blob-reused (their tar of the
/// working tree depends on commit + on-disk state), so we only assert
/// that the run still succeeds and that two distinct manifests exist.
#[test]
fn run_git_source_out_change_invalidates_manifest() {
    let fx = copy_from(&net_fixtures_root(), "net_git");
    let toml_path = fx.path.join("net_git").join("edo.toml");

    fx.edo(&["run", "//net_git/build"]).success();

    replace_toml_string_value(&toml_path, "out", ".", "src");

    fx.edo(&["run", "//net_git/build"]).success();

    // Git source manifests are named after the `url@ref` slug; the prefix
    // depends on the fixture URL. Look for any manifest containing
    // "Hello-World" (matches the `octocat/Hello-World` test repo) and
    // assert that the `out` field landed in `path_hints` for distinct
    // manifests.
    let path = fx.storage.join("storage").join("catalog.json");
    let raw = std::fs::read_to_string(&path).expect("catalog");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let manifests = v.get("manifests").and_then(|m| m.as_object()).unwrap();
    let git_hints: Vec<String> = manifests
        .iter()
        .filter(|(k, _)| k.contains("Hello_World"))
        .filter_map(|(_, m)| {
            m.get("config")
                .and_then(|c| c.get("path_hints"))
                .and_then(|p| p.as_object())
                .map(|obj| obj.values().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .map(|vs| vs.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
        })
        .flatten()
        .collect();
    assert!(
        git_hints.contains(&".".to_string()) && git_hints.contains(&"src".to_string()),
        "expected both `.` and `src` git path hints after `out` change, got {git_hints:?}"
    );
}
