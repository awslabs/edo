use edo_integration_tests::common::*;

#[test]
fn prune_all_empties_storage() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["run", "//hello_local/emit"]).success();

    assert!(
        has_any_contents(&fx.storage),
        "storage should be populated after a run, but {:?} is empty",
        fx.storage,
    );

    fx.edo(&["prune", "--all", "--logs"]).success();

    // Prune empties the local storage caches and the log directory. The
    // storage root itself may still exist (the backend keeps its directory
    // hierarchy), but nested artifact / layer files should be gone.
    let remaining = count_files(&fx.storage);
    assert_eq!(
        remaining, 0,
        "expected no files under {:?} after prune --all, found {remaining}",
        fx.storage,
    );
}

fn has_any_contents(root: &std::path::Path) -> bool {
    count_files(root) > 0
}

fn count_files(root: &std::path::Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files += 1;
            }
        }
    }
    files
}
