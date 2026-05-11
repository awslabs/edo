use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use tempfile::TempDir;

/// Absolute path to the offline fixture tree (`tests/fixtures/`).
pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Absolute path to the expected-failure fixture tree (`tests/error_fixtures/`).
pub fn error_fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("error_fixtures")
}

/// Absolute path to the network/container opt-in fixture tree (`tests/net_fixtures/`).
pub fn net_fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("net_fixtures")
}

/// A copy of a fixture placed in a temporary directory so tests cannot pollute each other.
///
/// `path` is the directory edo is invoked from — it is the parent of a copied
/// `<name>/edo.toml` subtree, which means the walker sees the fixture at
/// namespace `//<name>` (matching the umbrella layout). This lets individual
/// tests reference addresses like `//hello_local/emit` regardless of whether
/// the fixture was copied standalone or as part of the umbrella.
pub struct Fixture {
    pub dir: TempDir,
    pub path: PathBuf,
    pub storage: PathBuf,
}

impl Fixture {
    /// Returns the path to the lockfile edo will write on load.
    pub fn lock_path(&self) -> PathBuf {
        self.path.join("edo.lock.json")
    }

    /// Builds a bare `edo` invocation rooted at this fixture.
    ///
    /// Does not inject `--storage` — use [`Fixture::edo`] when you want that
    /// isolation (which is what every test default does).
    pub fn cmd(&self) -> Command {
        let mut c =
            Command::cargo_bin("edo-cli").expect("edo-cli binary should be built by dev-dep");
        c.current_dir(&self.path);
        c.env_remove("RUST_LOG");
        c
    }

    /// Invokes `edo --storage <fixture>/.edo-test-store <args...>` and returns
    /// the [`Assert`] for the caller to assert on.
    ///
    /// `--storage` must be passed as a global flag BEFORE the subcommand
    /// (see `Args` in `crates/cli/src/main.rs`).
    pub fn edo(&self, args: &[&str]) -> Assert {
        let mut c = self.cmd();
        c.arg("--storage").arg(&self.storage);
        for a in args {
            c.arg(a);
        }
        c.assert()
    }
}

/// Copies `fixtures_root()/<name>` into a fresh tempdir (preserving the name
/// as a subdir) and returns a [`Fixture`]. Edo addresses inside the fixture
/// are at namespace `//<name>` — the same as in the committed umbrella tree.
pub fn copy_fixture(name: &str) -> Fixture {
    copy_from(&fixtures_root(), name)
}

/// Copies `root/<name>` into `tempdir/<name>`.
pub fn copy_from(root: &Path, name: &str) -> Fixture {
    let src = root.join(name);
    let dir = TempDir::new().expect("create tempdir");
    let dst = dir.path().join(name);
    copy_dir(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    let path = dir.path().to_path_buf();
    let storage = path.join(".edo-test-store");
    Fixture { dir, path, storage }
}

/// Copies the entire `fixtures/` tree into a tempdir so umbrella / cross-project
/// tests can walk it.
pub fn copy_umbrella() -> Fixture {
    let src = fixtures_root();
    let dir = TempDir::new().expect("create tempdir");
    let path = dir.path().join("umbrella");
    copy_dir(&src, &path).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    let storage = path.join(".edo-test-store");
    Fixture { dir, path, storage }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
