//! Utility helpers shared across the crate.
//!
//! Provides [`Reader`] and [`Writer`] wrappers with integrated SHA256 hashing,
//! synchronous adapters for async I/O ([`SyncReader`], [`sync`], [`sync_fn`]),
//! filesystem helpers ([`copy_r`]), and subprocess execution functions that
//! pipe output through the build log.

mod command;
mod fs;
mod reader;
mod sync;
mod writer;

pub use command::*;
pub use fs::*;
pub use reader::*;
pub use sync::*;
pub use writer::*;
