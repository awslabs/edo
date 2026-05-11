use edo_integration_tests::common::*;

#[test]
fn update_writes_lockfile() {
    let fx = copy_fixture("hello_local");
    assert!(!fx.lock_path().exists(), "no lockfile before update");
    fx.edo(&["update"]).success();
    assert!(
        fx.lock_path().exists(),
        "edo update must create edo.lock.json at {}",
        fx.lock_path().display(),
    );
}

#[test]
fn run_is_locked_after_update() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["update"]).success();

    // `calculate_digest` in `crates/edo/src/context/builder.rs` only hashes
    // `[requires]` entries, which this offline fixture does not have. To
    // trigger the `DependencyChange` error on a locked run we directly
    // corrupt the recorded digest — this is exactly the state a user would
    // land in if their dependency graph changed after the lock was written.
    let lock_body = std::fs::read_to_string(fx.lock_path()).unwrap();
    let mut lock: serde_json::Value = serde_json::from_str(&lock_body).unwrap();
    lock["digest"] = serde_json::Value::String("tampered".into());
    std::fs::write(fx.lock_path(), serde_json::to_string(&lock).unwrap()).unwrap();

    fx.edo(&["run", "//hello_local/emit"]).failure();
}

#[test]
fn update_without_edits_then_run_ok() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["update"]).success();
    fx.edo(&["run", "//hello_local/emit"]).success();
    // Running again without edits must remain successful.
    fx.edo(&["run", "//hello_local/emit"]).success();
}
