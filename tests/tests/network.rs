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
