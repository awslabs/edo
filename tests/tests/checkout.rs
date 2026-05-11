use edo_integration_tests::common::*;

#[test]
fn checkout_extracts_files() {
    let fx = copy_fixture("hello_local");
    fx.edo(&["run", "//hello_local/emit"]).success();
    let out = fx.dir.path().join("out");
    fx.edo(&[
        "checkout",
        "//hello_local/emit",
        out.to_str().unwrap(),
    ])
    .success();
    let greeting = find_file(&out, "greeting.txt").expect("greeting.txt must exist");
    let content = std::fs::read_to_string(&greeting).unwrap();
    assert!(
        content.contains("hello from edo integration tests"),
        "unexpected content: {content:?}",
    );
}

#[test]
fn checkout_script_produces_hello_txt() {
    let fx = copy_fixture("hello_script");
    fx.edo(&["run", "//hello_script/build"]).success();
    let out = fx.dir.path().join("out");
    fx.edo(&[
        "checkout",
        "//hello_script/build",
        out.to_str().unwrap(),
    ])
    .success();
    let hello = find_file(&out, "hello.txt").expect("hello.txt must exist");
    let content = std::fs::read_to_string(&hello).unwrap();
    assert!(
        content.contains("script-produced hello"),
        "unexpected content: {content:?}",
    );
}

#[test]
fn checkout_compose_contains_all() {
    let fx = copy_fixture("hello_compose");
    fx.edo(&["run", "//hello_compose/bundle"]).success();
    let out = fx.dir.path().join("out");
    fx.edo(&[
        "checkout",
        "//hello_compose/bundle",
        out.to_str().unwrap(),
    ])
    .success();
    assert!(
        find_file(&out, "left.txt").is_some(),
        "left.txt missing from composed artifact",
    );
    assert!(
        find_file(&out, "right.txt").is_some(),
        "right.txt missing from composed artifact",
    );
}

fn find_file(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|x| x.to_str()) == Some(name) {
                return Some(path);
            }
        }
    }
    None
}
