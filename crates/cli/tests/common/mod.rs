//! Helpers shared by the CLI integration tests.
//!
//! Each test file compiles this module separately and uses a subset of it.
#![allow(dead_code, unused_imports)]

pub mod catalog;
pub mod fixtures;
pub mod network;

pub use catalog::{count_blobs, read_path_hints_by_prefix, replace_toml_string_value};
pub use fixtures::{
    Fixture, copy_fixture, copy_from, copy_umbrella, error_fixtures_root, fixtures_root,
    net_fixtures_root,
};
pub use network::container_enabled;
