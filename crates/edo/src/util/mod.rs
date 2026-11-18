//! Utility helpers shared across the crate.
//!
//! Provides [`Reader`] and [`Writer`] wrappers with integrated SHA256 hashing,
//! tar packing/extraction over async streams ([`archive`]),
//! filesystem helpers ([`copy_r`]), and subprocess execution functions that
//! pipe output through the build log.

pub mod archive;
mod command;
mod fs;
mod reader;
mod writer;

pub use command::*;
pub use fs::*;
pub use reader::*;
pub use writer::*;
