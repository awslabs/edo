//! Helpers for inspecting an isolated `--storage` directory written by an
//! integration test run.
//!
//! Tests use these to assert on cache invariants (e.g. "changing `out`
//! produces a fresh manifest", "the same blob is reused across two runs")
//! without depending on internal Rust types — they read the on-disk
//! `catalog.json` directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On-disk subdirectory under `--storage` that holds the local backend's
/// catalog and blob tree. Mirrors the CLI's storage-flag layout. Pull
/// this through every helper so a future layout change has one place to
/// update instead of many text-replaced literals.
pub fn local_storage_root(storage: &Path) -> PathBuf {
    storage.join("storage")
}

/// Returns `path_hints` (artifact-level table introduced when path hints
/// were lifted off `Layer`) for every manifest whose id starts with the
/// given `<prefix>:`.
///
/// The prefix is matched against `Id::name`, not the full `name:digest`
/// string, so e.g. `"files"` matches `"files:abc..."` even though `Id`s
/// embed both name and digest. The separator is `:` because the on-disk
/// id format is `<name>[.arch][@version]:<digest>`; using `-` (the
/// pre-refactor separator) would silently miss every manifest.
///
/// Used to verify that two runs of the same source with different `out`
/// values produced two distinct manifests carrying their respective hints.
pub fn read_path_hints_by_prefix(storage: &Path, prefix: &str) -> Vec<BTreeMap<String, String>> {
    let path = local_storage_root(storage).join("catalog.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&raw).expect("catalog json");
    let manifests = v
        .get("manifests")
        .and_then(|m| m.as_object())
        .expect("catalog.manifests is an object");
    manifests
        .iter()
        .filter(|(k, _)| k.starts_with(&format!("{prefix}:")))
        .map(|(_, manifest)| {
            manifest
                .get("config")
                .and_then(|c| c.get("path_hints"))
                .and_then(|p| p.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// Counts the number of blob files under `<storage>/storage/blobs/sha256`.
///
/// Filters to filenames matching the SHA256 digest format (64 hex chars)
/// so leftover `.tmp` files from in-flight or aborted writes do not cause
/// flaky assertions on regression tests that count blobs.
///
/// The remote-source blob-reuse fix should produce **exactly one** blob
/// for the same `ref` even after `out` changes; the local/git equivalents
/// are allowed to grow because they are not content-addressed at the
/// source level.
pub fn count_blobs(storage: &Path) -> usize {
    let dir = local_storage_root(storage).join("blobs").join("sha256");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read blob dir {}: {e}", dir.display()))
        .filter_map(|r| r.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|n| n.len() == 64 && n.chars().all(|c| c.is_ascii_hexdigit()))
                .unwrap_or(false)
        })
        .count()
}

/// Replace the value of a top-level TOML key (e.g. `out`) in-place using
/// a whitespace-tolerant regex. This avoids fixture tests being load-bearing
/// on the exact spacing used in committed `edo.toml` files.
///
/// `key` must be a simple identifier (e.g. `"out"`); the replacement
/// targets the first `<key> = "<value>"` line whose value matches
/// `expected_value`. Panics if no match is found, so a fixture rename
/// fails the test loudly rather than silently no-op'ing the patch.
pub fn replace_toml_string_value(
    toml_path: &Path,
    key: &str,
    expected_value: &str,
    new_value: &str,
) {
    let original = std::fs::read_to_string(toml_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", toml_path.display()));
    let pattern = format!(
        r#"(?m)^(\s*{}\s*=\s*)"{}"(\s*(?:#.*)?)$"#,
        regex::escape(key),
        regex::escape(expected_value),
    );
    let re = regex::Regex::new(&pattern).expect("valid regex");
    let replacement = format!(r#"$1"{}"$2"#, new_value);
    let modified = re.replace(&original, replacement.as_str()).into_owned();
    assert_ne!(
        original,
        modified,
        "no match for `{key} = \"{expected_value}\"` in {}; fixture format changed?",
        toml_path.display(),
    );
    std::fs::write(toml_path, modified)
        .unwrap_or_else(|e| panic!("write {}: {e}", toml_path.display()));
}
