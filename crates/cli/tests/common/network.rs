/// True when the test author has opted into container tests via
/// `EDO_TEST_CONTAINER=1` AND a container runtime is reachable on `PATH`.
pub fn container_enabled() -> bool {
    if std::env::var("EDO_TEST_CONTAINER").is_err() {
        return false;
    }
    which::which("podman").is_ok() || which::which("docker").is_ok()
}
